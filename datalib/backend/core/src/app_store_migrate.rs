//! The app stores' migration ladder (`datalib_store_meta::ladder`).
//!
//! The app stores are created with `CREATE TABLE IF NOT EXISTS`, which
//! leaves a table that already exists exactly as it was — so a rename in
//! the schema reaches an existing root only through a rung here. One
//! rung so far: the stamps we mint became `<x>_at_utc` beside a
//! `tz_offset`, and the old values were local-offset stamps rather than
//! UTC, so the rows are rewritten as well as the columns. The rung
//! probes for the old column, so a store made after the rename is at
//! version 0 and passes through it unchanged.
//!
//! One move, too: the disk timeseries left the doltlite `usage` store for
//! the plain-SQLite `disk_stats` one. [`carry_legacy_usage`] copies an old
//! root's rows across on open.

use std::path::Path;

use datalib_store_meta::Migration;
use sqlx::sqlite::SqlitePool;
use sqlx::{Row, SqliteConnection};

/// One table's stamp columns, old name to new, and the key that names
/// a row for the rewrite.
pub(crate) struct StampColumns {
    pub table: &'static str,
    pub key: &'static [&'static str],
    pub renames: &'static [(&'static str, &'static str)],
}

pub(crate) const FEEDBACK: StampColumns = StampColumns {
    table: "feedback",
    key: &["feedback_uuid"],
    renames: &[("created_at", "created_at_utc")],
};

pub(crate) const DISK_USAGE: StampColumns = StampColumns {
    table: "disk_usage",
    key: &["path", "measured_at_utc"],
    renames: &[("measured_at", "measured_at_utc")],
};

async fn column_names(
    conn: &mut SqliteConnection,
    table: &str,
) -> Result<Vec<String>, sqlx::Error> {
    // Safe: `table` is one of the literals above, never input.
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!("PRAGMA table_info({table})")))
        .fetch_all(&mut *conn)
        .await?;
    Ok(rows.iter().map(|r| r.get::<String, _>("name")).collect())
}

/// Rename the stamp columns a pre-#427 store still carries, add
/// `tz_offset`, and rewrite every row's stamps from the local-offset
/// form into UTC plus that offset. A store already in the new shape —
/// or one with no table yet — is left alone. Returns whether anything
/// was changed, so the caller can commit it.
pub(crate) async fn migrate_stamps(
    conn: &mut SqliteConnection,
    spec: &StampColumns,
) -> Result<bool, sqlx::Error> {
    let cols = column_names(conn, spec.table).await?;
    if cols.is_empty() {
        return Ok(false);
    }
    let has = |c: &str| cols.iter().any(|x| x == c);
    let renames: Vec<(&str, &str)> = spec
        .renames
        .iter()
        .copied()
        .filter(|(old, new)| has(old) && !has(new))
        .collect();
    if renames.is_empty() {
        return Ok(false);
    }
    for (old, new) in &renames {
        // Safe: every name here is a literal from the consts above.
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "ALTER TABLE {} RENAME COLUMN {old} TO {new}",
            spec.table
        )))
        .execute(&mut *conn)
        .await?;
    }
    if !has("tz_offset") {
        // Safe: `spec.table` is one of the literals above.
        sqlx::query(sqlx::AssertSqlSafe(format!(
            "ALTER TABLE {} ADD COLUMN tz_offset VARCHAR(8)",
            spec.table
        )))
        .execute(&mut *conn)
        .await?;
    }
    rewrite_rows(conn, spec, &renames).await?;
    Ok(true)
}

/// The renamed columns held stamps with the server's own offset. Each
/// becomes UTC, and the offset of the latest of them goes in
/// `tz_offset` — what a row written today would carry.
async fn rewrite_rows(
    conn: &mut SqliteConnection,
    spec: &StampColumns,
    renames: &[(&str, &str)],
) -> Result<(), sqlx::Error> {
    let new_cols: Vec<&str> = renames.iter().map(|(_, new)| *new).collect();
    let select_cols: Vec<&str> = spec
        .key
        .iter()
        .copied()
        .chain(new_cols.iter().copied())
        .collect();
    // Safe: the table and every column name are literals from the
    // `StampColumns` consts above.
    let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
        "SELECT {} FROM {}",
        select_cols.join(", "),
        spec.table
    )))
    .fetch_all(&mut *conn)
    .await?;
    for row in rows {
        let key: Vec<String> = spec.key.iter().map(|k| row.get::<String, _>(k)).collect();
        let mut sets: Vec<(String, String)> = Vec::new();
        let mut latest: Option<(String, String)> = None;
        for col in &new_cols {
            let Some(old) = row.get::<Option<String>, _>(col) else {
                continue;
            };
            let Ok(t) = datalib_time::parse_strict(&old) else {
                // Not a stamp we can read: leave it, and it stays as
                // it was, which is still better than losing the row.
                continue;
            };
            let (utc, offset) = t.to_utc_and_offset();
            if latest.as_ref().is_none_or(|(u, _)| *u < utc) {
                latest = Some((utc.clone(), offset));
            }
            sets.push((col.to_string(), utc));
        }
        if sets.is_empty() {
            continue;
        }
        if let Some((_, offset)) = latest {
            sets.push(("tz_offset".to_string(), offset));
        }
        // Safe: column names are literals; every value is bound.
        let assignments: Vec<String> = sets.iter().map(|(c, _)| format!("{c} = ?")).collect();
        let wheres: Vec<String> = spec.key.iter().map(|k| format!("{k} = ?")).collect();
        let mut q = sqlx::query(sqlx::AssertSqlSafe(format!(
            "UPDATE {} SET {} WHERE {}",
            spec.table,
            assignments.join(", "),
            wheres.join(" AND ")
        )));
        for (_, v) in &sets {
            q = q.bind(v);
        }
        for k in &key {
            q = q.bind(k);
        }
        q.execute(&mut *conn).await?;
    }
    Ok(())
}

/// Rung 1 of every app store's ladder, one per store because a rung
/// captures nothing.
macro_rules! stamps_rung {
    ($spec:expr) => {
        Migration {
            version: 1,
            name: "stamps to utc + tz_offset",
            apply: |conn| {
                Box::pin(async move {
                    migrate_stamps(conn, &$spec).await?;
                    Ok(())
                })
            },
        }
    };
}

pub(crate) const FEEDBACK_LADDER: &[Migration] = &[stamps_rung!(FEEDBACK)];
/// Born as plain SQLite after the stamp rename; an old root's rows reach
/// it through [`carry_legacy_usage`], which climbs that rung on the way.
pub(crate) const DISK_STATS_LADDER: &[Migration] = &[];
/// Born after the stamp rename; nothing to climb yet.
pub(crate) const REMOTE_MEDIA_LADDER: &[Migration] = &[];

/// How many rows one page of the copy reads.
const COPY_PAGE: i64 = 5_000;

/// Copy the rows of the doltlite `usage` store a build before
/// `disk_stats` left into `to`, then remove the old file. Its columns are
/// first brought to today's names, since it may still owe the stamp rung.
/// A copy that fails leaves the old file for the next open to try again;
/// every row is keyed, so a second copy adds nothing twice. The number of
/// rows copied, 0 when there was no old file.
pub(crate) async fn carry_legacy_usage(root: &Path, to: &SqlitePool) -> Result<u64, sqlx::Error> {
    let legacy = crate::layout::legacy_usage_db(root);
    if !legacy.is_file() {
        return Ok(0);
    }
    let from = crate::store::open_pool(&legacy).await?;
    let copied = copy_usage_rows(&from, to).await;
    from.close().await;
    let copied = copied?;
    remove_with_sidecars(&legacy).map_err(sqlx::Error::Io)?;
    Ok(copied)
}

async fn copy_usage_rows(from: &SqlitePool, to: &SqlitePool) -> Result<u64, sqlx::Error> {
    let mut src = from.acquire().await?;
    migrate_stamps(&mut src, &DISK_USAGE).await?;
    let tables: Vec<String> =
        sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table'")
            .fetch_all(&mut *src)
            .await?;
    let has = |t: &str| tables.iter().any(|n| n == t);
    let mut copied = 0u64;
    let mut tx = to.begin().await?;
    if has("disk_usage") {
        let mut after: (String, String) = (String::new(), String::new());
        loop {
            let rows = sqlx::query(
                "SELECT path, measured_at_utc, tz_offset, bytes FROM disk_usage \
                 WHERE path > ? OR (path = ? AND measured_at_utc > ?) \
                 ORDER BY path, measured_at_utc LIMIT ?",
            )
            .bind(&after.0)
            .bind(&after.0)
            .bind(&after.1)
            .bind(COPY_PAGE)
            .fetch_all(&mut *src)
            .await?;
            let Some(last) = rows.last() else { break };
            after = (last.get("path"), last.get("measured_at_utc"));
            for r in &rows {
                sqlx::query(
                    "INSERT OR IGNORE INTO disk_usage (path, measured_at_utc, tz_offset, bytes) \
                     VALUES (?, ?, ?, ?)",
                )
                .bind(r.get::<String, _>("path"))
                .bind(r.get::<String, _>("measured_at_utc"))
                .bind(r.get::<Option<String>, _>("tz_offset"))
                .bind(r.get::<i64, _>("bytes"))
                .execute(&mut *tx)
                .await?;
            }
            copied += rows.len() as u64;
        }
    }
    if has("disk_free") {
        let mut after = String::new();
        loop {
            let rows = sqlx::query(
                "SELECT measured_at_utc, tz_offset, available_bytes, total_bytes FROM disk_free \
                 WHERE measured_at_utc > ? ORDER BY measured_at_utc LIMIT ?",
            )
            .bind(&after)
            .bind(COPY_PAGE)
            .fetch_all(&mut *src)
            .await?;
            let Some(last) = rows.last() else { break };
            after = last.get("measured_at_utc");
            for r in &rows {
                sqlx::query(
                    "INSERT OR IGNORE INTO disk_free \
                     (measured_at_utc, tz_offset, available_bytes, total_bytes) VALUES (?, ?, ?, ?)",
                )
                .bind(r.get::<String, _>("measured_at_utc"))
                .bind(r.get::<Option<String>, _>("tz_offset"))
                .bind(r.get::<i64, _>("available_bytes"))
                .bind(r.get::<i64, _>("total_bytes"))
                .execute(&mut *tx)
                .await?;
            }
            copied += rows.len() as u64;
        }
    }
    tx.commit().await?;
    Ok(copied)
}

/// The store and whatever the engine kept beside it under its name.
fn remove_with_sidecars(store: &Path) -> std::io::Result<()> {
    let (Some(dir), Some(name)) = (store.parent(), store.file_name()) else {
        return Ok(());
    };
    let name = name.to_string_lossy().into_owned();
    for entry in std::fs::read_dir(dir)?.flatten() {
        if entry.file_name().to_string_lossy().starts_with(&name) {
            std::fs::remove_file(entry.path())?;
        }
    }
    Ok(())
}
