//! The grid's search terms file,
//! `unified_document_index/grid_index/search_terms.sqlite`: every term each
//! `grid_rows` row answers to (`datalib_schema::search_terms`), kept in step
//! with the grid index after each `grid_index` pass. It is the grid's
//! rows melted, one indexed row per term: a pure function of `grid_rows`
//! at one commit, what each row's columns say (`search_terms_of`) and what
//! its render supplied (`supplied_terms`). The file records the grid
//! generation it reflects and the last `grid_changes` entry it applied,
//! moves forward by the entries after that, or is built whole when it
//! cannot.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use datalib_schema::search_terms::{
    decode_supplied, search_terms_of, SearchTerm, SearchTermSource, META_GRID_CHANGE,
    META_GRID_GENERATION, META_SHAPE, TERMS_DDL, TERMS_SHAPE,
};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::SqliteConnection;

/// What a sync has to do, from what the file records and where the grid is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// The file already reflects the grid's generation.
    Current,
    /// Build the file whole: it records nothing, another shape, or another
    /// grid file (a generation `<file id>.<count>` with another id).
    Whole,
    /// Apply the rows of the `grid_changes` after this one.
    Since(i64),
}

/// What the file records: the grid generation, and the last change applied.
#[derive(Debug, Clone, Default)]
pub struct Recorded {
    pub generation: Option<String>,
    pub change: Option<i64>,
    pub shape: Option<String>,
}

pub fn plan(recorded: &Recorded, generation: &str) -> Plan {
    let file_id = |g: &str| g.split_once('.').map(|(id, _)| id.to_string());
    match (
        &recorded.generation,
        recorded.change,
        recorded.shape.as_deref(),
    ) {
        (Some(g), _, Some(TERMS_SHAPE)) if g == generation => Plan::Current,
        (Some(g), Some(change), Some(TERMS_SHAPE)) if file_id(g) == file_id(generation) => {
            Plan::Since(change)
        }
        _ => Plan::Whole,
    }
}

/// What one sync did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Synced {
    /// `current`, `whole` or `since`.
    pub plan: &'static str,
    /// Grid rows whose terms were written again (or removed).
    pub rows: usize,
    /// Terms written.
    pub terms: usize,
    /// The last `grid_changes` entry the file reflects.
    pub through: i64,
}

/// Bring the search terms file at `terms_path` to the grid index's
/// generation. Run by the grid's one writer between its passes, so the
/// grid holds still while it reads; it then deletes the changes it applied.
pub async fn sync(grid: &SqlitePool, terms_path: &Path) -> Result<Synced> {
    let Some(generation) = datalib_etl::pin::generation(grid).await? else {
        return Ok(Synced::default());
    };
    let terms = open_in_shape(terms_path).await?;
    let result = sync_open(grid, &terms, &generation).await;
    // On the error path too: dropping the pool only schedules the close.
    let closed = crate::grid_index::close_wal_file(terms, "the search terms file").await;
    let synced = result?;
    closed?;
    sqlx::query("DELETE FROM grid_changes WHERE seq <= ?")
        .bind(synced.through)
        .execute(grid)
        .await
        .context("forget the grid changes the terms applied")?;
    Ok(synced)
}

/// The file, made new when it was built in another shape: it holds
/// nothing a pass cannot write again.
async fn open_in_shape(path: &Path) -> Result<SqlitePool> {
    let pool = open_terms(path).await?;
    let mut conn = pool.acquire().await?;
    let has_meta: bool = sqlx::query_scalar(
        "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = 'terms_meta'",
    )
    .fetch_one(&mut *conn)
    .await?;
    let shape = if has_meta {
        meta(&mut conn, META_SHAPE).await?
    } else {
        None
    };
    if shape.as_deref().is_none_or(|s| s == TERMS_SHAPE) {
        create(&mut conn).await?;
        drop(conn);
        return Ok(pool);
    }
    drop(conn);
    tracing::info!(
        was = shape.as_deref().unwrap_or(""),
        now = TERMS_SHAPE,
        "the search terms file is in another shape; building it again"
    );
    pool.close().await;
    // Its log too: a `-wal` left beside a new file would be read into it.
    for suffix in ["", "-wal", "-shm"] {
        let file = PathBuf::from(format!("{}{suffix}", path.display()));
        match std::fs::remove_file(&file) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            other => other.with_context(|| format!("remove {}", file.display()))?,
        }
    }
    let pool = open_terms(path).await?;
    let mut conn = pool.acquire().await?;
    create(&mut conn).await?;
    drop(conn);
    Ok(pool)
}

async fn sync_open(grid: &SqlitePool, terms: &SqlitePool, generation: &str) -> Result<Synced> {
    let mut conn = terms
        .acquire()
        .await
        .context("acquire the search terms file")?;
    let synced = sync_terms(grid, &mut conn, generation).await?;
    if synced.plan != "current" {
        write_people(grid, &mut conn).await?;
    }
    Ok(synced)
}

/// Each handle with every name it was seen under: the author a grid row
/// shows beside its handle, and every name a source's record of a person
/// (`source_contacts`) gives for each handle it ties to them; and the
/// handles of each source's own account. Written whole on each pass that
/// moves the terms: it is a few thousand rows.
async fn write_people(grid: &SqlitePool, conn: &mut SqliteConnection) -> Result<()> {
    let authors: Vec<(String, String)> = sqlx::query_as(
        "SELECT DISTINCT author_handle, author FROM grid_rows \
         WHERE author_handle IS NOT NULL AND author_handle != '' \
         AND author IS NOT NULL AND author != ''",
    )
    .fetch_all(grid)
    .await
    .context("read the grid's authors")?;
    let contacts: Vec<(String, String)> = sqlx::query_as(
        "SELECT h.handle, c.contact_json FROM source_contact_handles h \
         JOIN source_contacts c \
         ON c.markdown_uuid = h.markdown_uuid AND c.contact_key = h.contact_key",
    )
    .fetch_all(grid)
    .await
    .context("read the source contacts")?;
    let accounts = account_handles(&contacts);
    let names = names_seen_under(authors, &contacts);
    let mut tx = sqlx::Connection::begin(&mut *conn).await?;
    sqlx::query("DELETE FROM names").execute(&mut *tx).await?;
    let pairs: Vec<&(String, String)> = names.iter().collect();
    for chunk in pairs.chunks(CHUNK) {
        // Audited: a placeholder pair per name, every value bound.
        let sql = format!(
            "INSERT INTO names (handle, name) VALUES {}",
            placeholders(chunk.len(), 2)
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for (handle, name) in chunk {
            q = q.bind(handle).bind(name);
        }
        q.execute(&mut *tx).await.context("write the names")?;
    }
    sqlx::query("DELETE FROM account_handles")
        .execute(&mut *tx)
        .await?;
    for (handle, source_id) in &accounts {
        sqlx::query("INSERT INTO account_handles (handle, source_id) VALUES (?, ?)")
            .bind(handle)
            .bind(source_id)
            .execute(&mut *tx)
            .await
            .context("write the account handles")?;
    }
    tx.commit().await?;
    Ok(())
}

/// The `(handle, source_id)` pairs [`write_people`] keeps as the
/// account's own: each handle of a source contact marked as the account
/// (`NormalizedContact::account`), beside the source that says so.
pub fn account_handles(
    contacts: &[(String, String)],
) -> std::collections::BTreeSet<(String, String)> {
    contacts
        .iter()
        // `account` is written only when true; most records skip the parse.
        .filter(|(_, json)| json.contains("\"account\""))
        .filter_map(|(handle, json)| {
            let contact: datalib_contact_schema::NormalizedContact =
                serde_json::from_str(json).ok()?;
            contact.account.then(|| (handle.clone(), contact.source_id))
        })
        .collect()
}

/// The `(handle, name)` pairs [`write_people`] keeps: the grid's authors,
/// then each source contact's names (`NormalizedContact::names`) for each
/// of its handles. A record this build cannot read gives no names.
pub fn names_seen_under(
    authors: Vec<(String, String)>,
    contacts: &[(String, String)],
) -> std::collections::BTreeSet<(String, String)> {
    let mut out: std::collections::BTreeSet<(String, String)> = std::collections::BTreeSet::new();
    let mut unread = 0usize;
    for (handle, name) in authors {
        out.insert((handle, name.trim().to_string()));
    }
    for (handle, json) in contacts {
        match serde_json::from_str::<datalib_contact_schema::NormalizedContact>(json) {
            Ok(contact) => {
                for name in contact.names {
                    out.insert((handle.clone(), name.trim().to_string()));
                }
            }
            Err(_) => unread += 1,
        }
    }
    if unread > 0 {
        tracing::warn!(
            unread,
            "source contacts this build cannot read gave no names"
        );
    }
    out.retain(|(_, name)| !name.is_empty());
    out
}

async fn sync_terms(
    grid: &SqlitePool,
    conn: &mut SqliteConnection,
    generation: &str,
) -> Result<Synced> {
    let recorded = Recorded {
        generation: meta(&mut *conn, META_GRID_GENERATION).await?,
        change: meta(&mut *conn, META_GRID_CHANGE)
            .await?
            .and_then(|c| c.parse().ok()),
        shape: meta(&mut *conn, META_SHAPE).await?,
    };
    let through: i64 = sqlx::query_scalar("SELECT coalesce(max(seq), 0) FROM grid_changes")
        .fetch_one(grid)
        .await
        .context("read the grid's last change")?;
    let at = At {
        generation,
        through,
    };
    match plan(&recorded, generation) {
        Plan::Current => Ok(Synced {
            plan: "current",
            through: recorded.change.unwrap_or(0).max(through),
            ..Synced::default()
        }),
        Plan::Since(from) => {
            let uuids: Vec<String> = sqlx::query_scalar(
                "SELECT DISTINCT uuid FROM grid_changes WHERE seq > ? AND seq <= ?",
            )
            .bind(from)
            .bind(through)
            .fetch_all(grid)
            .await
            .context("read the grid's changes")?;
            let rows = rows_by_uuid(grid, &uuids).await?;
            let terms = write(conn, Some(&uuids), &rows, &at).await?;
            Ok(Synced {
                plan: "since",
                rows: uuids.len(),
                terms,
                through,
            })
        }
        Plan::Whole => whole(grid, conn, &at).await,
    }
}

/// The grid as a sync found it: its generation, and its last change.
struct At<'a> {
    generation: &'a str,
    through: i64,
}

async fn whole(grid: &SqlitePool, conn: &mut SqliteConnection, at: &At<'_>) -> Result<Synced> {
    // Audited: the column list is a literal.
    let rows: Vec<SearchTermSource> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {} FROM grid_rows",
        SearchTermSource::COLUMNS
    )))
    .fetch_all(grid)
    .await
    .context("read grid_rows for the terms")?;
    let terms = write(conn, None, &rows, at).await?;
    Ok(Synced {
        plan: "whole",
        rows: rows.len(),
        terms,
        through: at.through,
    })
}

/// The rows behind `uuids` that the grid still holds; a removed row has
/// none, and only loses its terms.
async fn rows_by_uuid(grid: &SqlitePool, uuids: &[String]) -> Result<Vec<SearchTermSource>> {
    let mut out = Vec::new();
    for chunk in uuids.chunks(CHUNK) {
        // Audited: a placeholder per value, every value bound.
        let sql = format!(
            "SELECT {} FROM grid_rows WHERE uuid IN ({})",
            SearchTermSource::COLUMNS,
            placeholders(chunk.len(), 1)
        );
        let mut q = sqlx::query_as::<_, SearchTermSource>(sqlx::AssertSqlSafe(sql));
        for uuid in chunk {
            q = q.bind(uuid);
        }
        out.extend(q.fetch_all(grid).await.context("read changed grid_rows")?);
    }
    Ok(out)
}

/// Every term of `rows`: what each derives from its columns, then what
/// its render supplied.
fn every_term(rows: &[SearchTermSource]) -> Result<Vec<(&SearchTermSource, SearchTerm)>> {
    let mut out = Vec::new();
    for row in rows {
        let supplied = match row.supplied_terms.as_deref() {
            Some(json) => decode_supplied(json)
                .map_err(anyhow::Error::msg)
                .with_context(|| format!("grid row {}", row.uuid))?,
            None => Vec::new(),
        };
        out.extend(
            search_terms_of(row)
                .into_iter()
                .chain(supplied)
                .map(|t| (row, t)),
        );
    }
    Ok(out)
}

/// One transaction: drop the rows `replace` names (every row, for `None`)
/// with their terms, write `rows`' terms, drop the values no term uses any
/// more, and record `at`. Returns the terms written.
async fn write(
    conn: &mut SqliteConnection,
    replace: Option<&[String]>,
    rows: &[SearchTermSource],
    at: &At<'_>,
) -> Result<usize> {
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
    let written = async {
        // The batch is staged first, so the dictionary tables fill by
        // joins rather than a lookup per term. `dropped` holds the values
        // the removed rows used: the only ones that can have lost their
        // last term.
        for sql in [
            "CREATE TEMP TABLE IF NOT EXISTS incoming (uuid TEXT NOT NULL, \
             touched_at_utc TEXT, kind INTEGER NOT NULL, value TEXT NOT NULL)",
            "CREATE TEMP TABLE IF NOT EXISTS dropped (val_id INTEGER PRIMARY KEY)",
            "DELETE FROM temp.incoming",
            "DELETE FROM temp.dropped",
        ] {
            sqlx::query(sql).execute(&mut *conn).await?;
        }
        match replace {
            None => {
                for sql in [
                    "INSERT INTO vals_fts (vals_fts) VALUES ('delete-all')",
                    "DELETE FROM terms",
                    "DELETE FROM vals",
                    "DELETE FROM rows",
                ] {
                    sqlx::query(sql).execute(&mut *conn).await?;
                }
            }
            Some(uuids) => {
                for chunk in uuids.chunks(CHUNK) {
                    let p = placeholders(chunk.len(), 1);
                    for sql in [
                        format!(
                            "INSERT OR IGNORE INTO temp.dropped SELECT t.val_id FROM terms t \
                             JOIN rows r ON r.row_id = t.row_id WHERE r.uuid IN ({p})"
                        ),
                        format!(
                            "DELETE FROM terms WHERE row_id IN \
                             (SELECT row_id FROM rows WHERE uuid IN ({p}))"
                        ),
                        format!("DELETE FROM rows WHERE uuid IN ({p})"),
                    ] {
                        // Audited: a placeholder per value, every value bound.
                        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
                        for uuid in chunk {
                            q = q.bind(uuid);
                        }
                        q.execute(&mut *conn).await?;
                    }
                }
            }
        }
        let terms = every_term(rows)?;
        for chunk in terms.chunks(CHUNK) {
            // Audited: four placeholders per term, every value bound.
            let sql = format!(
                "INSERT INTO temp.incoming (uuid, touched_at_utc, kind, value) VALUES {}",
                placeholders(chunk.len(), 4)
            );
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
            for (row, term) in chunk {
                q = q
                    .bind(&row.uuid)
                    .bind(&row.touched_at_utc)
                    .bind(i64::from(term.kind.code()))
                    .bind(&term.value);
            }
            q.execute(&mut *conn).await?;
        }
        // New values take keys above every one there now, so the index
        // picks up exactly these.
        let floor: i64 = sqlx::query_scalar("SELECT coalesce(max(val_id), 0) FROM vals")
            .fetch_one(&mut *conn)
            .await?;
        for sql in [
            "INSERT INTO rows (uuid, touched_at_utc) \
             SELECT uuid, max(touched_at_utc) FROM temp.incoming GROUP BY uuid",
            // `WHERE true`: an upsert's SELECT needs a WHERE to parse.
            "INSERT INTO vals (value) SELECT DISTINCT value FROM temp.incoming WHERE true \
             ON CONFLICT (value) DO NOTHING",
        ] {
            sqlx::query(sql).execute(&mut *conn).await?;
        }
        sqlx::query(
            "INSERT INTO vals_fts (rowid, value) SELECT val_id, value FROM vals WHERE val_id > ?",
        )
        .bind(floor)
        .execute(&mut *conn)
        .await?;
        for sql in [
            "INSERT OR IGNORE INTO terms (val_id, kind, row_id) \
             SELECT v.val_id, i.kind, r.row_id FROM temp.incoming i \
             JOIN vals v ON v.value = i.value JOIN rows r ON r.uuid = i.uuid",
            "DELETE FROM temp.dropped WHERE val_id IN (SELECT val_id FROM terms)",
            "DELETE FROM vals_fts WHERE rowid IN (SELECT val_id FROM temp.dropped)",
            "DELETE FROM vals WHERE val_id IN (SELECT val_id FROM temp.dropped)",
        ] {
            sqlx::query(sql).execute(&mut *conn).await?;
        }
        let through = at.through.to_string();
        for (key, value) in [
            (META_GRID_GENERATION, at.generation),
            (META_GRID_CHANGE, through.as_str()),
            (META_SHAPE, TERMS_SHAPE),
        ] {
            sqlx::query(
                "INSERT INTO terms_meta (key, value) VALUES (?, ?) \
                 ON CONFLICT (key) DO UPDATE SET value = excluded.value",
            )
            .bind(key)
            .bind(value)
            .execute(&mut *conn)
            .await?;
        }
        anyhow::Ok(terms.len())
    }
    .await;
    match written {
        Ok(n) => {
            sqlx::query("COMMIT").execute(&mut *conn).await?;
            Ok(n)
        }
        Err(e) => {
            let _ = sqlx::query("ROLLBACK").execute(&mut *conn).await;
            Err(e).context("write the search terms file")
        }
    }
}

/// Values per statement, under SQLite's bound-parameter limit at four a
/// term.
const CHUNK: usize = 500;

fn placeholders(rows: usize, per_row: usize) -> String {
    let one = if per_row == 1 {
        "?".to_string()
    } else {
        format!("({})", vec!["?"; per_row].join(", "))
    };
    vec![one; rows].join(", ")
}

async fn open_terms(path: &Path) -> Result<SqlitePool> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let opts = SqliteConnectOptions::new()
        .filename(datalib_core::plain_sqlite::uri(path))
        .create_if_missing(true)
        // A sync that rewrites much of the file spills more pages than the
        // cache holds; with a rollback journal each spill waited on a
        // journal sync, and a reader's transaction held off its commit.
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(30));
    SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts)
        .await
        .with_context(|| format!("open {}", path.display()))
}

async fn create(conn: &mut SqliteConnection) -> Result<()> {
    for ddl in TERMS_DDL {
        sqlx::query(*ddl)
            .execute(&mut *conn)
            .await
            .with_context(|| format!("create: {ddl}"))?;
    }
    Ok(())
}

async fn meta(conn: &mut SqliteConnection, key: &str) -> Result<Option<String>> {
    Ok(
        sqlx::query_scalar::<_, String>("SELECT value FROM terms_meta WHERE key = ?")
            .bind(key)
            .fetch_optional(&mut *conn)
            .await?,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid_index::{apply_one, delete_markdown, open_index, RenderedMarkdown, WriteLock};
    use datalib_schema::grid_rows::GridRow;
    use datalib_schema::providers::Provider;
    use datalib_schema::search_terms::SearchTermKind;

    /// The account's handles are those of the source contacts marked as
    /// the account, every handle each ties to them; no other record's.
    #[test]
    fn the_accounts_handles_are_its_marked_contacts() {
        use datalib_contact_schema::{ContactKind, NormalizedContact};
        let mut picard = NormalizedContact::new("slack", "U_PICARD", ContactKind::Person);
        picard.account = true;
        let picard = serde_json::to_string(&picard).unwrap();
        let riker = NormalizedContact::new("slack", "U_RIKER", ContactKind::Person);
        let riker = serde_json::to_string(&riker).unwrap();
        let accounts = account_handles(&[
            ("slack:T/U_PICARD".into(), picard.clone()),
            ("email:picard@e.org".into(), picard),
            ("slack:T/U_RIKER".into(), riker),
            ("email:x@e.org".into(), "{not a contact \"account\"".into()),
        ]);
        let pairs: Vec<(&str, &str)> = accounts
            .iter()
            .map(|(h, s)| (h.as_str(), s.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("email:picard@e.org", "slack"),
                ("slack:T/U_PICARD", "slack")
            ]
        );
    }

    /// A handle is known by every name it was seen under: what its rows
    /// show as their author, and each name a source's record of the person
    /// gives; a blank name, or a record this build cannot read, adds none.
    #[test]
    fn a_handle_is_known_by_every_name_it_was_seen_under() {
        use datalib_contact_schema::{ContactKind, NormalizedContact};
        let mut riker = NormalizedContact::new("slack", "U_RIKER", ContactKind::Person);
        riker.names = vec!["Will Riker".into(), "  ".into(), "Number One".into()];
        let json = serde_json::to_string(&riker).unwrap();
        let names = names_seen_under(
            vec![("email:riker@e.org".into(), "William Riker".into())],
            &[
                ("slack:T/U_RIKER".into(), json.clone()),
                ("email:riker@e.org".into(), json),
                ("email:x@e.org".into(), "{not a contact".into()),
            ],
        );
        let pairs: Vec<(&str, &str)> = names
            .iter()
            .map(|(h, n)| (h.as_str(), n.as_str()))
            .collect();
        assert_eq!(
            pairs,
            [
                ("email:riker@e.org", "Number One"),
                ("email:riker@e.org", "Will Riker"),
                ("email:riker@e.org", "William Riker"),
                ("slack:T/U_RIKER", "Number One"),
                ("slack:T/U_RIKER", "Will Riker"),
            ]
        );
    }

    /// A document of one row: its uuid, its author's handle, its title.
    fn doc(root: &Path, uuid: &str, handle: &str, title: &str) -> RenderedMarkdown {
        let md_path = crate::grid_index::doc_md_path(&root.join("enterprise"), uuid);
        let file = md_path.strip_prefix(root).unwrap().to_string_lossy();
        let row = GridRow::builder()
            .uuid(uuid)
            .provider(Provider::Claude)
            .kind("Chat")
            .source_label("Claude")
            .is_document(true)
            .created_at(Some("2026-01-01T09:00:00+00:00".to_string()))
            .conversation_uuid(uuid)
            .conversation_name(Some(title.to_string()))
            .author_handle(Some(handle.to_string()))
            .entire_chat(format!("/chat/{uuid}"))
            .body("")
            .markdown_uuid(Some(uuid.to_string()))
            .qmd_path(file.into_owned())
            .build()
            .unwrap();
        RenderedMarkdown {
            markdown_uuid: uuid.to_string(),
            source_id: "enterprise".into(),
            upstream_cursor: None,
            bucket_key: None,
            md_path,
            render_version: 1,
            rows: vec![row],
            sections: Vec::new(),
            edges: Vec::new(),
            contacts: Vec::new(),
            problems: Vec::new(),
        }
    }

    struct Grid {
        _dir: tempfile::TempDir,
        root: std::path::PathBuf,
        pool: SqlitePool,
        terms: std::path::PathBuf,
    }

    impl Grid {
        async fn new() -> Grid {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().to_path_buf();
            let pool = open_index(&datalib_core::layout::grid_index_db(&root))
                .await
                .unwrap();
            let terms = datalib_core::layout::search_terms_db(&root);
            Grid {
                _dir: dir,
                root,
                pool,
                terms,
            }
        }

        async fn seal(&self, put: &[RenderedMarkdown], remove: &[&str]) {
            let lock = WriteLock::new(self.pool.clone());
            for md in put {
                apply_one(&lock, &self.root, md).await.unwrap();
            }
            for md in remove {
                delete_markdown(&lock, md).await.unwrap();
            }
            crate::grid_index::seal(&self.pool).await.unwrap();
        }

        /// The `(uuid, kind)` of every term matching `value` exactly.
        async fn matching(&self, value: &str) -> Vec<(String, String)> {
            let terms = open_terms(&self.terms).await.unwrap();
            let coded: Vec<(String, i64)> = sqlx::query_as(
                "SELECT r.uuid, t.kind FROM vals_fts JOIN terms t ON t.val_id = vals_fts.rowid \
                 JOIN rows r ON r.row_id = t.row_id WHERE vals_fts MATCH ?",
            )
            .bind(format!("\"{value}\""))
            .fetch_all(&terms)
            .await
            .unwrap();
            terms.close().await;
            let mut hits: Vec<(String, String)> = coded
                .into_iter()
                .map(|(uuid, code)| {
                    let kind = SearchTermKind::from_code(code).expect("a known kind");
                    (uuid, kind.as_str().to_string())
                })
                .collect();
            hits.sort();
            hits
        }

        /// Whether the dictionary still holds `value` at all.
        async fn holds(&self, value: &str) -> bool {
            let terms = open_terms(&self.terms).await.unwrap();
            let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM vals WHERE value = ?")
                .bind(value)
                .fetch_one(&terms)
                .await
                .unwrap();
            terms.close().await;
            n > 0
        }

        async fn set_meta(&self, key: &str, value: &str) {
            let terms = open_terms(&self.terms).await.unwrap();
            sqlx::query("UPDATE terms_meta SET value = ? WHERE key = ?")
                .bind(value)
                .bind(key)
                .execute(&terms)
                .await
                .unwrap();
            terms.close().await;
        }
    }

    fn hit(uuid: &str, kind: &str) -> (String, String) {
        (uuid.to_string(), kind.to_string())
    }

    #[tokio::test]
    async fn the_first_sync_builds_the_file_whole_and_the_next_finds_it_current() {
        let g = Grid::new().await;
        let (a, b) = (
            doc(&g.root, "c-a", "email:ann@example.com", "Away team"),
            doc(&g.root, "c-b", "email:bo@example.com", "Bridge"),
        );
        g.seal(&[a, b], &[]).await;

        let first = sync(&g.pool, &g.terms).await.unwrap();
        assert_eq!((first.plan, first.rows), ("whole", 2));
        assert_eq!(
            g.matching("email:ann@example.com").await,
            [hit("c-a", "from")]
        );
        assert_eq!(g.matching("c-b").await, [hit("c-b", "id")]);
        assert_eq!(sync(&g.pool, &g.terms).await.unwrap().plan, "current");
    }

    /// The point of recording the commit: a pass rewrites the terms of the
    /// rows that changed, and no others.
    #[tokio::test]
    async fn a_later_sync_rewrites_only_the_rows_that_changed() {
        let g = Grid::new().await;
        g.seal(
            &[
                doc(&g.root, "c-a", "email:ann@example.com", "Away team"),
                doc(&g.root, "c-b", "email:bo@example.com", "Bridge"),
                doc(&g.root, "c-k", "email:kit@example.com", "Kept"),
            ],
            &[],
        )
        .await;
        sync(&g.pool, &g.terms).await.unwrap();

        g.seal(
            &[
                doc(&g.root, "c-a", "email:ann@example.org", "Away team"),
                doc(&g.root, "c-c", "email:cy@example.com", "Cargo"),
            ],
            &["c-b"],
        )
        .await;
        let next = sync(&g.pool, &g.terms).await.unwrap();
        assert_eq!((next.plan, next.rows), ("since", 3));
        assert_eq!(g.matching("email:ann@example.com").await, Vec::new());
        assert_eq!(
            g.matching("email:ann@example.org").await,
            [hit("c-a", "from")]
        );
        assert_eq!(g.matching("c-b").await, Vec::new());
        assert_eq!(
            g.matching("email:cy@example.com").await,
            [hit("c-c", "from")]
        );
        assert_eq!(
            g.matching("email:kit@example.com").await,
            [hit("c-k", "from")]
        );
        assert!(
            !g.holds("email:ann@example.com").await,
            "a value no row uses any more stays"
        );
        assert!(!g.holds("Bridge").await, "a removed row's title stays");
        assert!(g.holds("Kept").await);
    }

    fn supplied(md: &mut RenderedMarkdown, terms: &[(SearchTermKind, &str)]) {
        md.rows[0].set_supplied_terms(terms.iter().map(|(k, v)| SearchTerm::new(*k, *v)));
    }

    /// A render's own terms (an email's To, its labels) reach the file,
    /// and a pass in which only they changed rewrites them: they are part
    /// of the row, so the row changed.
    #[tokio::test]
    async fn a_renders_supplied_terms_are_written_and_follow_its_rerender() {
        use SearchTermKind::{Cc, Label, To};
        let g = Grid::new().await;
        let mut a = doc(&g.root, "c-a", "email:ann@example.com", "Away team");
        supplied(&mut a, &[(To, "email:bo@example.com"), (Label, "Inbox")]);
        g.seal(&[a.clone()], &[]).await;
        sync(&g.pool, &g.terms).await.unwrap();
        assert_eq!(g.matching("email:bo@example.com").await, [hit("c-a", "to")]);
        assert_eq!(g.matching("Inbox").await, [hit("c-a", "label")]);

        supplied(
            &mut a,
            &[(To, "email:bo@example.com"), (Cc, "email:cy@example.com")],
        );
        g.seal(&[a], &[]).await;
        let next = sync(&g.pool, &g.terms).await.unwrap();
        assert_eq!((next.plan, next.rows), ("since", 1));
        assert_eq!(g.matching("Inbox").await, Vec::new());
        assert_eq!(g.matching("email:cy@example.com").await, [hit("c-a", "cc")]);
        assert_eq!(g.matching("email:bo@example.com").await, [hit("c-a", "to")]);

        g.seal(&[], &["c-a"]).await;
        sync(&g.pool, &g.terms).await.unwrap();
        assert_eq!(g.matching("email:bo@example.com").await, Vec::new());
    }

    /// Another grid file, or a file in another shape, cannot be moved
    /// forward; it is built again.
    #[tokio::test]
    async fn another_grid_file_or_another_shape_builds_the_file_whole() {
        let g = Grid::new().await;
        g.seal(&[doc(&g.root, "c-a", "email:ann@example.com", "Away")], &[])
            .await;
        sync(&g.pool, &g.terms).await.unwrap();

        g.set_meta(META_GRID_GENERATION, "0000000000000000.1").await;
        let again = sync(&g.pool, &g.terms).await.unwrap();
        assert_eq!((again.plan, again.rows), ("whole", 1));

        g.set_meta(META_SHAPE, "0").await;
        assert_eq!(sync(&g.pool, &g.terms).await.unwrap().plan, "whole");
        assert_eq!(
            g.matching("email:ann@example.com").await,
            [hit("c-a", "from")]
        );
    }

    #[test]
    fn the_plan_follows_what_the_file_records() {
        let recorded = |g: &str, change: Option<i64>, shape: &str| Recorded {
            generation: Some(g.to_string()),
            change,
            shape: Some(shape.to_string()),
        };
        assert_eq!(
            plan(&recorded("f.3", Some(9), TERMS_SHAPE), "f.3"),
            Plan::Current
        );
        assert_eq!(
            plan(&recorded("f.2", Some(9), TERMS_SHAPE), "f.3"),
            Plan::Since(9)
        );
        assert_eq!(
            plan(&recorded("e.2", Some(9), TERMS_SHAPE), "f.3"),
            Plan::Whole,
            "another grid file"
        );
        assert_eq!(
            plan(&recorded("f.2", None, TERMS_SHAPE), "f.3"),
            Plan::Whole
        );
        assert_eq!(plan(&Recorded::default(), "f.3"), Plan::Whole);
        assert_eq!(plan(&recorded("f.3", Some(9), "0"), "f.3"), Plan::Whole);
    }
}
