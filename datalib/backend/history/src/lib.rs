//! A doltlite store's commit history: `dolt_log` walked back from HEAD
//! along first parents, with what each commit did to each table.
//!
//! Read-only. The row counts come from one `COUNT(*)` per table at HEAD
//! and are walked backwards through each commit's `dolt_diff_stat`, so a
//! commit costs time proportional to what it changed rather than to the
//! size of the store.

use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use serde::Serialize;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

#[derive(Debug, Clone, Serialize)]
pub struct StoreHistory {
    /// Newest first.
    pub commits: Vec<Commit>,
    /// The store has more commits than were walked.
    pub truncated: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct Commit {
    pub hash: String,
    /// First parent; absent on the root commit.
    pub parent: Option<String>,
    pub committer: String,
    /// ISO-8601 with an explicit `+00:00`: doltlite stamps commits in UTC
    /// at second resolution and writes no offset.
    pub date: String,
    pub message: String,
    /// Every table the store has, as it stood after this commit, largest
    /// first. A table that did not exist yet and was not touched is left
    /// out.
    pub tables: Vec<TableState>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TableState {
    pub table: String,
    /// Rows after this commit.
    pub rows: i64,
    pub added: i64,
    pub deleted: i64,
    pub modified: i64,
}

pub async fn read(db_path: &Path, limit: usize) -> Result<StoreHistory> {
    let pool = open_reader(db_path).await?;
    let result = read_from(&pool, limit).await;
    pool.close().await;
    result
}

async fn read_from(pool: &SqlitePool, limit: usize) -> Result<StoreHistory> {
    let head: Option<String> = sqlx::query_scalar("SELECT dolt_hashof('HEAD')")
        .fetch_optional(pool)
        .await
        .context("dolt_hashof('HEAD')")?;
    let Some(head) = head else {
        return Ok(StoreHistory {
            commits: Vec::new(),
            truncated: false,
        });
    };

    let mut log: BTreeMap<String, (String, String, String)> = BTreeMap::new();
    for row in sqlx::query("SELECT commit_hash, committer, date, message FROM dolt_log()")
        .fetch_all(pool)
        .await
        .context("dolt_log()")?
    {
        log.insert(
            row.get("commit_hash"),
            (row.get("committer"), row.get("date"), row.get("message")),
        );
    }
    let mut parents: BTreeMap<String, String> = BTreeMap::new();
    for row in sqlx::query(
        "SELECT commit_hash, parent_hash FROM dolt_commit_ancestors WHERE parent_index = 0",
    )
    .fetch_all(pool)
    .await
    .context("dolt_commit_ancestors")?
    {
        let parent: String = row.get("parent_hash");
        if !parent.is_empty() {
            parents.insert(row.get("commit_hash"), parent);
        }
    }

    let mut sizes = table_sizes(pool).await?;
    let mut commits = Vec::new();
    let mut cursor = Some(head);
    while let Some(hash) = cursor {
        if commits.len() == limit {
            return Ok(StoreHistory {
                commits,
                truncated: true,
            });
        }
        let (committer, date, message) = log
            .get(&hash)
            .cloned()
            .with_context(|| format!("commit {hash} is HEAD's ancestor but not in dolt_log()"))?;
        let parent = parents.get(&hash).cloned();
        let changes = match &parent {
            Some(parent) => table_changes(pool, parent, &hash).await?,
            None => BTreeMap::new(),
        };
        let mut tables: Vec<TableState> = sizes
            .iter()
            .map(|(table, rows)| {
                let (added, deleted, modified) = changes.get(table).copied().unwrap_or((0, 0, 0));
                TableState {
                    table: table.clone(),
                    rows: *rows,
                    added,
                    deleted,
                    modified,
                }
            })
            .filter(|t| t.rows != 0 || t.added != 0 || t.deleted != 0 || t.modified != 0)
            .collect();
        tables.sort_by(|a, b| b.rows.cmp(&a.rows).then_with(|| a.table.cmp(&b.table)));
        for (table, (added, deleted, _)) in &changes {
            let rows = sizes.entry(table.clone()).or_insert(0);
            *rows = *rows - added + deleted;
        }
        commits.push(Commit {
            hash,
            parent: parent.clone(),
            committer,
            date: iso_utc(&date),
            message,
            tables,
        });
        cursor = parent;
    }
    Ok(StoreHistory {
        commits,
        truncated: false,
    })
}

/// Row count of every user table at HEAD.
async fn table_sizes(pool: &SqlitePool) -> Result<BTreeMap<String, i64>> {
    let names: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name NOT LIKE 'sqlite%' ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .context("sqlite_master")?;
    let mut sizes = BTreeMap::new();
    for name in names {
        // The identifier is quoted, and it came from sqlite_master rather
        // than from a request.
        let sql = format!("SELECT COUNT(*) FROM \"{}\"", name.replace('"', "\"\""));
        let rows: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
            .fetch_one(pool)
            .await
            .with_context(|| format!("count {name}"))?;
        sizes.insert(name, rows);
    }
    Ok(sizes)
}

/// `(added, deleted, modified)` per table with a data change between the
/// two commits. Per table rather than the two-argument `dolt_diff_stat`,
/// which aborts the whole result on `sqlite_sequence`.
async fn table_changes(
    pool: &SqlitePool,
    from: &str,
    to: &str,
) -> Result<BTreeMap<String, (i64, i64, i64)>> {
    let changed: Vec<String> = sqlx::query(
        "SELECT from_table_name, to_table_name FROM dolt_diff_summary \
          WHERE from_ref = ?1 AND to_ref = ?2 AND data_change = 1",
    )
    .bind(from)
    .bind(to)
    .fetch_all(pool)
    .await
    .with_context(|| format!("dolt_diff_summary {from}..{to}"))?
    .into_iter()
    .map(|row| {
        let to_name: String = row.get("to_table_name");
        if to_name.is_empty() {
            row.get("from_table_name")
        } else {
            to_name
        }
    })
    .filter(|name| !name.starts_with("sqlite"))
    .collect();
    let mut changes = BTreeMap::new();
    for table in changed {
        let stat = sqlx::query(
            "SELECT rows_added, rows_deleted, rows_modified FROM dolt_diff_stat(?1, ?2, ?3)",
        )
        .bind(from)
        .bind(to)
        .bind(&table)
        .fetch_optional(pool)
        .await
        .with_context(|| format!("dolt_diff_stat {from}..{to} {table}"))?;
        if let Some(row) = stat {
            changes.insert(
                table,
                (
                    row.get("rows_added"),
                    row.get("rows_deleted"),
                    row.get("rows_modified"),
                ),
            );
        }
    }
    Ok(changes)
}

fn iso_utc(dolt_date: &str) -> String {
    match dolt_date.split_once(' ') {
        Some((day, clock)) if !dolt_date.contains('T') => format!("{day}T{clock}+00:00"),
        _ => dolt_date.to_string(),
    }
}

/// Connect read-only, and never create: an absent store is the caller's
/// error, not a first run. Cold opens of multi-GB stores take seconds,
/// hence the long acquire timeout.
async fn open_reader(db_path: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))
        .with_context(|| format!("sqlite uri for {}", db_path.display()))?
        .create_if_missing(false)
        .read_only(true);
    SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .acquire_timeout(Duration::from_secs(300))
        .connect_with(opts)
        .await
        .with_context(|| format!("open {} read-only", db_path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn writer(path: &Path) -> SqlitePool {
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .unwrap()
            .create_if_missing(true);
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap()
    }

    async fn is_doltlite(pool: &SqlitePool) -> bool {
        let n: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM pragma_function_list WHERE name = 'dolt_commit'",
        )
        .fetch_one(pool)
        .await
        .unwrap();
        n > 0
    }

    async fn commit(pool: &SqlitePool, msg: &str) {
        sqlx::query("SELECT dolt_commit('-Am', ?)")
            .bind(msg)
            .execute(pool)
            .await
            .unwrap();
    }

    fn table<'a>(c: &'a Commit, name: &str) -> &'a TableState {
        c.tables
            .iter()
            .find(|t| t.table == name)
            .unwrap_or_else(|| panic!("{name} missing from {:?}", c.tables))
    }

    /// Row counts are measured once at HEAD and walked back through each
    /// commit's diff, so every commit's `rows` must equal what the table
    /// held right after it — including across a delete.
    #[tokio::test]
    async fn walks_row_counts_back_from_head() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.doltlite_db");
        let pool = writer(&path).await;
        if !is_doltlite(&pool).await {
            return;
        }
        sqlx::query("CREATE TABLE big (id INTEGER PRIMARY KEY, v TEXT)")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("CREATE TABLE small (id INTEGER PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();
        commit(&pool, "schema").await;
        for i in 0..5 {
            sqlx::query("INSERT INTO big (id, v) VALUES (?, 'a')")
                .bind(i)
                .execute(&pool)
                .await
                .unwrap();
        }
        sqlx::query("INSERT INTO small (id) VALUES (1)")
            .execute(&pool)
            .await
            .unwrap();
        commit(&pool, "first load").await;
        sqlx::query("DELETE FROM big WHERE id < 2")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("UPDATE big SET v = 'b' WHERE id = 4")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO big (id, v) VALUES (9, 'c')")
            .execute(&pool)
            .await
            .unwrap();
        commit(&pool, "churn").await;
        pool.close().await;

        let h = read(&path, 100).await.unwrap();
        assert!(!h.truncated);
        let messages: Vec<&str> = h.commits.iter().map(|c| c.message.as_str()).collect();
        assert_eq!(
            messages,
            [
                "churn",
                "first load",
                "schema",
                "Initialize data repository"
            ]
        );
        assert!(
            h.commits[0].date.ends_with("+00:00"),
            "{}",
            h.commits[0].date
        );
        assert_eq!(
            h.commits[0].parent.as_deref(),
            Some(h.commits[1].hash.as_str())
        );
        assert_eq!(h.commits[3].parent, None);

        let churn = &h.commits[0];
        assert_eq!(
            churn.tables[0].table, "big",
            "largest first: {:?}",
            churn.tables
        );
        let big = table(churn, "big");
        assert_eq!(
            (big.rows, big.added, big.deleted, big.modified),
            (4, 1, 2, 1)
        );
        let small = table(churn, "small");
        assert_eq!((small.rows, small.added, small.deleted), (1, 0, 0));

        let first = &h.commits[1];
        let big = table(first, "big");
        assert_eq!(
            (big.rows, big.added, big.deleted, big.modified),
            (5, 5, 0, 0)
        );
        assert_eq!(table(first, "small").rows, 1);

        // Before any rows existed, nothing is worth listing.
        assert!(h.commits[2].tables.is_empty(), "{:?}", h.commits[2].tables);
        assert!(h.commits[3].tables.is_empty());
    }

    #[tokio::test]
    async fn limit_marks_the_walk_as_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("store.doltlite_db");
        let pool = writer(&path).await;
        if !is_doltlite(&pool).await {
            return;
        }
        sqlx::query("CREATE TABLE t (id INTEGER PRIMARY KEY)")
            .execute(&pool)
            .await
            .unwrap();
        commit(&pool, "one").await;
        sqlx::query("INSERT INTO t (id) VALUES (1)")
            .execute(&pool)
            .await
            .unwrap();
        commit(&pool, "two").await;
        pool.close().await;

        let h = read(&path, 2).await.unwrap();
        assert!(h.truncated);
        assert_eq!(h.commits.len(), 2);
        assert_eq!(h.commits[0].message, "two");
        assert_eq!(table(&h.commits[0], "t").rows, 1);
    }

    #[test]
    fn dolt_dates_get_an_explicit_utc_offset() {
        assert_eq!(iso_utc("2026-09-08 20:54:34"), "2026-09-08T20:54:34+00:00");
        assert_eq!(
            iso_utc("2026-09-08T20:54:34+02:00"),
            "2026-09-08T20:54:34+02:00"
        );
    }
}
