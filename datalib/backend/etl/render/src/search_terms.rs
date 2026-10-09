//! The grid's search terms file,
//! `unified_index/grid_index/search_terms.sqlite`: every term each
//! `grid_rows` row answers to (`datalib_schema::search_terms`), kept in step
//! with the grid index after each `grid_index` pass. It is a pure
//! function of `grid_rows` and `supplied_search_terms` at one commit: the
//! file records the commit it reflects and moves to the next by
//! `dolt_diff`, or is built whole when it cannot. Plain SQLite, so it keeps no history of itself.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use datalib_schema::search_terms::{
    search_terms_of, SearchTerm, SearchTermKind, SearchTermSource, META_GRID_COMMIT, META_SHAPE,
    TERMS_DDL, TERMS_SHAPE,
};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::SqliteConnection;

/// What a sync has to do, from what the file records and the grid's head.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Plan {
    /// The file already reflects the head.
    Current,
    /// Build the file whole: it records no commit, or another shape.
    Whole,
    /// Apply the rows that changed since this commit.
    Since(String),
}

pub fn plan(recorded_commit: Option<&str>, recorded_shape: Option<&str>, head: &str) -> Plan {
    match (recorded_commit, recorded_shape) {
        (Some(commit), Some(TERMS_SHAPE)) if commit == head => Plan::Current,
        (Some(commit), Some(TERMS_SHAPE)) => Plan::Since(commit.to_string()),
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
}

/// Bring the search terms file at `terms_path` to the grid index's head. A store
/// without doltlite has no head, and no terms.
pub async fn sync(grid: &SqlitePool, terms_path: &Path) -> Result<Synced> {
    let Some(head) = datalib_etl::doltlite_raw::head_commit(grid).await? else {
        return Ok(Synced::default());
    };
    let terms = open_in_shape(terms_path).await?;
    let result = sync_open(grid, &terms, &head).await;
    // On the error path too: dropping the pool only schedules the close.
    terms.close().await;
    result
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
    std::fs::remove_file(path).with_context(|| format!("remove {}", path.display()))?;
    let pool = open_terms(path).await?;
    let mut conn = pool.acquire().await?;
    create(&mut conn).await?;
    drop(conn);
    Ok(pool)
}

async fn sync_open(grid: &SqlitePool, terms: &SqlitePool, head: &str) -> Result<Synced> {
    let mut conn = terms
        .acquire()
        .await
        .context("acquire the search terms file")?;
    let recorded = meta(&mut conn, META_GRID_COMMIT).await?;
    let shape = meta(&mut conn, META_SHAPE).await?;
    match plan(recorded.as_deref(), shape.as_deref(), head) {
        Plan::Current => Ok(Synced {
            plan: "current",
            ..Synced::default()
        }),
        Plan::Since(from) => {
            match datalib_etl::doltlite_raw::changed_keys(grid, "grid_rows", &from, head).await {
                Ok(changed) => {
                    let mut uuids: Vec<String> = changed.into_iter().map(|k| k.key).collect();
                    uuids.extend(changed_supplied_rows(grid, &from, head).await?);
                    uuids.sort();
                    uuids.dedup();
                    let rows = rows_by_uuid(grid, &uuids).await?;
                    let supplied = supplied_by_uuid(grid, &uuids).await?;
                    let terms = write(&mut conn, Some(&uuids), &rows, &supplied, head).await?;
                    Ok(Synced {
                        plan: "since",
                        rows: uuids.len(),
                        terms,
                    })
                }
                // A commit the grid no longer has, say after a rebuild:
                // the whole file is the one answer still right.
                Err(e) => {
                    tracing::warn!(
                        from = %from,
                        error = %format!("{e:#}"),
                        "could not diff the grid index from the commit the terms \
                         reflect; building the search terms file whole"
                    );
                    whole(grid, &mut conn, head).await
                }
            }
        }
        Plan::Whole => whole(grid, &mut conn, head).await,
    }
}

async fn whole(grid: &SqlitePool, conn: &mut SqliteConnection, head: &str) -> Result<Synced> {
    // Audited: the column list is a literal.
    let rows: Vec<SearchTermSource> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT {} FROM grid_rows",
        SearchTermSource::COLUMNS
    )))
    .fetch_all(grid)
    .await
    .context("read grid_rows for the terms")?;
    let supplied: Vec<Supplied> =
        sqlx::query_as("SELECT uuid, kind, value FROM supplied_search_terms")
            .fetch_all(grid)
            .await
            .context("read the supplied search terms")?;
    let terms = write(conn, None, &rows, &supplied, head).await?;
    Ok(Synced {
        plan: "whole",
        rows: rows.len(),
        terms,
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

/// The rows whose supplied search terms changed between the two commits.
/// A key of that table is `uuid|kind|value`, and a uuid holds no `|`.
async fn changed_supplied_rows(grid: &SqlitePool, from: &str, head: &str) -> Result<Vec<String>> {
    let changed =
        datalib_etl::doltlite_raw::changed_keys(grid, "supplied_search_terms", from, head).await?;
    Ok(changed
        .into_iter()
        .filter_map(|k| k.key.split('|').next().map(str::to_string))
        .collect())
}

/// A `supplied_search_terms` row, as the file needs it.
#[derive(Debug, Clone, sqlx::FromRow)]
struct Supplied {
    uuid: String,
    kind: String,
    value: String,
}

async fn supplied_by_uuid(grid: &SqlitePool, uuids: &[String]) -> Result<Vec<Supplied>> {
    let mut out = Vec::new();
    for chunk in uuids.chunks(CHUNK) {
        // Audited: a placeholder per value, every value bound.
        let sql = format!(
            "SELECT uuid, kind, value FROM supplied_search_terms WHERE uuid IN ({})",
            placeholders(chunk.len(), 1)
        );
        let mut q = sqlx::query_as::<_, Supplied>(sqlx::AssertSqlSafe(sql));
        for uuid in chunk {
            q = q.bind(uuid);
        }
        out.extend(
            q.fetch_all(grid)
                .await
                .context("read changed supplied search terms")?,
        );
    }
    Ok(out)
}

/// Every term of `rows`: what each derives, and what renders supplied for
/// it. A supplied term whose row the grid does not hold is no term.
fn every_term<'a>(
    rows: &'a [SearchTermSource],
    supplied: &'a [Supplied],
) -> Result<Vec<(&'a SearchTermSource, SearchTerm)>> {
    let by_uuid: std::collections::HashMap<&str, &SearchTermSource> =
        rows.iter().map(|r| (r.uuid.as_str(), r)).collect();
    let mut out: Vec<(&SearchTermSource, SearchTerm)> = rows
        .iter()
        .flat_map(|row| search_terms_of(row).into_iter().map(move |t| (row, t)))
        .collect();
    for term in supplied {
        let Some(row) = by_uuid.get(term.uuid.as_str()) else {
            continue;
        };
        let kind = SearchTermKind::parse(&term.kind).with_context(|| {
            format!(
                "a supplied search term of a kind this build does not know: {:?}",
                term.kind
            )
        })?;
        out.push((
            row,
            SearchTerm {
                kind,
                value: term.value.clone(),
            },
        ));
    }
    Ok(out)
}

/// One transaction: drop the rows `replace` names (every row, for `None`)
/// with their terms, write `rows`' terms, drop the values no term uses any
/// more, and record `head`. Returns the terms written.
async fn write(
    conn: &mut SqliteConnection,
    replace: Option<&[String]>,
    rows: &[SearchTermSource],
    supplied: &[Supplied],
    head: &str,
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
        let terms = every_term(rows, supplied)?;
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
        for (key, value) in [(META_GRID_COMMIT, head), (META_SHAPE, TERMS_SHAPE)] {
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

    /// A document of one row: its uuid, its author's handle, its title.
    fn doc(root: &Path, uuid: &str, handle: &str, title: &str) -> RenderedMarkdown {
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
            .build()
            .unwrap();
        RenderedMarkdown {
            markdown_uuid: uuid.to_string(),
            source_id: "enterprise".into(),
            upstream_cursor: None,
            bucket_key: None,
            md_path: root.join(format!("enterprise/{uuid}.md")),
            render_version: 1,
            rows: vec![row],
            sections: Vec::new(),
            search_terms: Vec::new(),
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
            datalib_etl::doltlite_raw::commit_run(&self.pool, "pass")
                .await
                .unwrap();
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

    fn supplied(md: &mut RenderedMarkdown, kind: SearchTermKind, value: &str) {
        md.search_terms
            .push(datalib_schema::search_terms::SuppliedSearchTerm {
                uuid: md.markdown_uuid.clone(),
                kind,
                value: value.to_string(),
            });
    }

    /// A render's own terms (an email's To, its labels) reach the file,
    /// and a pass in which only they changed still rewrites them: the
    /// row itself did not move, so `grid_rows`' diff alone would miss it.
    #[tokio::test]
    async fn a_renders_supplied_terms_are_written_and_follow_its_rerender() {
        let g = Grid::new().await;
        let mut a = doc(&g.root, "c-a", "email:ann@example.com", "Away team");
        supplied(&mut a, SearchTermKind::To, "email:bo@example.com");
        supplied(&mut a, SearchTermKind::Label, "Inbox");
        g.seal(&[a.clone()], &[]).await;
        sync(&g.pool, &g.terms).await.unwrap();
        assert_eq!(g.matching("email:bo@example.com").await, [hit("c-a", "to")]);
        assert_eq!(g.matching("Inbox").await, [hit("c-a", "label")]);

        a.search_terms.retain(|t| t.kind != SearchTermKind::Label);
        supplied(&mut a, SearchTermKind::Cc, "email:cy@example.com");
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

    /// A term names one of its document's rows, or it is a render bug the
    /// search would carry silently.
    #[tokio::test]
    async fn a_term_for_a_row_the_document_does_not_have_is_refused() {
        let g = Grid::new().await;
        let mut a = doc(&g.root, "c-a", "email:ann@example.com", "Away team");
        supplied(&mut a, SearchTermKind::To, "email:bo@example.com");
        a.search_terms[0].uuid = "m-elsewhere".into();
        let lock = WriteLock::new(g.pool.clone());
        let err = apply_one(&lock, &g.root, &a).await.unwrap_err();
        assert!(format!("{err:#}").contains("m-elsewhere"), "{err:#}");
    }

    /// A commit the grid no longer has, or a file in another shape, cannot
    /// be moved forward; it is built again.
    #[tokio::test]
    async fn an_unknown_commit_or_another_shape_builds_the_file_whole() {
        let g = Grid::new().await;
        g.seal(&[doc(&g.root, "c-a", "email:ann@example.com", "Away")], &[])
            .await;
        sync(&g.pool, &g.terms).await.unwrap();

        g.set_meta(META_GRID_COMMIT, &"0".repeat(40)).await;
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
        assert_eq!(plan(Some("h"), Some(TERMS_SHAPE), "h"), Plan::Current);
        assert_eq!(
            plan(Some("a"), Some(TERMS_SHAPE), "h"),
            Plan::Since("a".into())
        );
        assert_eq!(plan(None, None, "h"), Plan::Whole);
        assert_eq!(plan(Some("h"), Some("0"), "h"), Plan::Whole);
    }
}
