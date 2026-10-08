//! Where a file-backed source got to: its resume cursor, per feed.
//!
//! The persistent half of "which files changed since this feed last finished
//! with them?" — [`crate::fsscan`] is the other half, and the crate README
//! explains why they live apart, why the cursor is a content hash rather than
//! a stat pair, and what that does not fix.
//!
//! Each scope namespaces rows per `(provider, feed)`, so two feeds can claim
//! the same file without colliding.

use std::collections::HashSet;

use anyhow::{Context, Result};
use sqlx::{Sqlite, SqlitePool, Transaction};

use crate::fsscan::{FileScanCursor, ScannedFile};

/// The cursor table. One row per `(scope, root-relative path)`.
///
/// Scope names should be `"<provider>/<feed>"` (e.g.
/// `"google_takeout/maps_reviews"`); collisions across providers are
/// the caller's responsibility to avoid.
pub const INGESTED_FILES_TABLE: &str = "ingested_files";

pub const INGESTED_FILES_DDL: &str = "CREATE TABLE IF NOT EXISTS ingested_files (
    scope TEXT NOT NULL,
    rel_path TEXT NOT NULL,
    blake3 TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    last_finished_at_utc TEXT NOT NULL,
    tz_offset TEXT NULL,
    PRIMARY KEY (scope, rel_path)
)";

/// Create the table, dropping one written to an older shape.
///
/// A store from before the cursor became content-based has `mtime_ns` where
/// `blake3` now goes, and no migration can invent hashes it never recorded.
/// Dropping is cheap: this is a cursor, so losing it costs one re-ingest.
pub async fn ensure_schema(pool: &SqlitePool) -> Result<()> {
    let cols: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('ingested_files')")
            .fetch_all(pool)
            .await
            .context("inspect ingested_files")?;
    let current = cols.iter().any(|c| c == "blake3") && cols.iter().any(|c| c == "rel_path");
    if !cols.is_empty() && !current {
        sqlx::query("DROP TABLE ingested_files")
            .execute(pool)
            .await
            .context("drop outdated ingested_files")?;
        tracing::info!(
            event = "ingested_files_reset",
            "cursor table predates content hashing; dropped so the next run re-reads",
        );
    }
    sqlx::query(INGESTED_FILES_DDL)
        .execute(pool)
        .await
        .context("create ingested_files")?;
    Ok(())
}

/// This scope's cursor: what each file hashed to when this feed last
/// finished with it.
///
/// Feed it straight to [`crate::fsscan::Scan::changes_since`] — that
/// pair is the whole "what changed since I last looked?" question.
pub async fn load_cursor(pool: &SqlitePool, scope: &str) -> Result<FileScanCursor> {
    ensure_schema(pool).await?;
    let rows = sqlx::query_as::<_, (String, String)>(
        "SELECT rel_path, blake3 FROM ingested_files WHERE scope = ?",
    )
    .bind(scope)
    .fetch_all(pool)
    .await
    .with_context(|| format!("load ingested_files scope={scope}"))?;
    Ok(rows
        .into_iter()
        .filter_map(|(rel, hex)| crate::fsscan::from_hex(&hex).map(|h| (rel, h)))
        .collect())
}

/// Stamp one scanned file as finished, inside the caller's transaction.
///
/// Per file, not per run, so a crash partway through a directory keeps what
/// landed and re-reads only the rest. Callers run this in the same transaction
/// that wrote the file's rows, so a crash between the two cannot leave a stamp
/// claiming content that never arrived. A file recorded again with the same
/// bytes keeps its stamp, so a source that reads an unchanged file again
/// commits nothing for it.
pub async fn record_file(
    tx: &mut Transaction<'_, Sqlite>,
    scope: &str,
    file: &ScannedFile,
) -> Result<()> {
    record_file_with_problem(tx, scope, file, None).await
}

/// [`record_file`], and what this read of the file could not use: a
/// record that would not parse, one with no key. The file is stamped, so
/// nothing reads it again until it changes; its problem is a row keyed
/// `file:<scope>:<rel>` that only the next stamp of it rewrites and only
/// [`forget_file`] drops. `None` is a clean read and clears the row.
pub async fn record_file_with_problem(
    tx: &mut Transaction<'_, Sqlite>,
    scope: &str,
    file: &ScannedFile,
    problem: Option<(datalib_problems::Outcome, datalib_problems::Problem)>,
) -> Result<()> {
    replace_file_problem(tx, scope, &file.rel, problem).await?;
    let (now, tz_offset) = datalib_time::IsoOffsetTimestamp::now_local().to_utc_and_offset();
    sqlx::query(
        "INSERT INTO ingested_files \
            (scope, rel_path, blake3, size_bytes, last_finished_at_utc, tz_offset)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT(scope, rel_path) DO UPDATE SET
            blake3 = excluded.blake3,
            size_bytes = excluded.size_bytes,
            last_finished_at_utc = excluded.last_finished_at_utc,
            tz_offset = excluded.tz_offset
         WHERE ingested_files.blake3 != excluded.blake3
            OR ingested_files.size_bytes != excluded.size_bytes",
    )
    .bind(scope)
    .bind(&file.rel)
    .bind(crate::fsscan::hex(&file.blake3))
    .bind(file.size)
    .bind(&now)
    .bind(&tz_offset)
    .execute(&mut **tx)
    .await
    .with_context(|| format!("upsert ingested_files {scope}={}", file.rel))?;
    Ok(())
}

/// Drop one path's stamp, inside the transaction that deleted the rows it
/// produced — so a crash between the two leaves the path still stamped,
/// and the next run sees it removed again and retries.
pub async fn forget_file(tx: &mut Transaction<'_, Sqlite>, scope: &str, rel: &str) -> Result<()> {
    replace_file_problem(tx, scope, rel, None).await?;
    sqlx::query("DELETE FROM ingested_files WHERE scope = ? AND rel_path = ?")
        .bind(scope)
        .bind(rel)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("forget ingested_files {scope}={rel}"))?;
    Ok(())
}

fn file_problem_key(scope: &str, rel: &str) -> String {
    format!("{FILE_PROBLEM_PREFIX}{scope}:{rel}")
}

/// The sweep key's prefix of every row [`record_file_with_problem`] writes.
pub const FILE_PROBLEM_PREFIX: &str = "file:";

/// Set or clear one file's problem row, stamped by `ProblemRow::stamped`.
async fn replace_file_problem(
    tx: &mut Transaction<'_, Sqlite>,
    scope: &str,
    rel: &str,
    problem: Option<(datalib_problems::Outcome, datalib_problems::Problem)>,
) -> Result<()> {
    use datalib_etl::bulk::BulkUpsertable as _;
    use datalib_problems::{ProblemRow, Scope, ScopeKind, Stage};
    let key = file_problem_key(scope, rel);
    let earlier = sqlx::query("SELECT * FROM problems WHERE scope_kind = ? AND scope_key = ?")
        .bind(ScopeKind::Entity.as_str())
        .bind(&key)
        .fetch_optional(&mut **tx)
        .await
        .with_context(|| format!("read the problem of {key}"))?
        .map(|r| ProblemRow::from_row(&r))
        .transpose()?;
    sqlx::query("DELETE FROM problems WHERE scope_kind = ? AND scope_key = ?")
        .bind(ScopeKind::Entity.as_str())
        .bind(&key)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("clear the problem of {key}"))?;
    let Some((outcome, problem)) = problem else {
        return Ok(());
    };
    let (now, tz_offset) = datalib_time::IsoOffsetTimestamp::now_local().to_utc_and_offset();
    let row = ProblemRow::new(
        "",
        Stage::Fetch,
        Scope::Entity(&key),
        None,
        outcome,
        problem,
        None,
    )
    .stamped(earlier.as_ref(), &now, Some(&tz_offset));
    let sql = datalib_etl::bulk::insert_sql::<ProblemRow>();
    // Audited: `sql` is built from `ProblemRow`'s associated consts, never
    // from row data; all values bound.
    row.bind_into(sqlx::query(sqlx::AssertSqlSafe(sql)))
        .execute(&mut **tx)
        .await
        .with_context(|| format!("record the problem of {key}"))?;
    datalib_problems::note_recorded([&row]);
    Ok(())
}

/// [`forget_file`] for several paths, in a transaction of its own: for a
/// caller whose deletion already committed, where a crash in between only
/// means the next run finds the same paths gone and deletes nothing more.
pub async fn forget_files(pool: &SqlitePool, scope: &str, rels: &[&str]) -> Result<()> {
    if rels.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await.context("begin forget_files tx")?;
    for rel in rels {
        forget_file(&mut tx, scope, rel).await?;
    }
    tx.commit().await.context("commit forget_files tx")?;
    Ok(())
}

/// [`record_file`] for callers that don't already own a transaction.
pub async fn record_file_pool(pool: &SqlitePool, scope: &str, file: &ScannedFile) -> Result<()> {
    let mut tx = pool.begin().await.context("begin record_file tx")?;
    record_file(&mut tx, scope, file).await?;
    tx.commit().await.context("commit record_file tx")?;
    Ok(())
}

/// What [`ingest_snapshot`] did to its table.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct SnapshotCounts {
    pub written: usize,
    pub removed: usize,
}

/// Mirror one already-scanned file that holds the whole of `T`'s table, if
/// its contents have changed since `scope` last finished with it: upsert
/// what it lists, and delete the rows it no longer lists, in one
/// transaction.
///
/// Takes a [`ScannedFile`] rather than a path because the provider has already
/// scanned the export root, so the file's existence and its hash are known.
///
/// `parse` returns every record the file holds, and an empty list
/// empties the table. A file it cannot read as a whole list — not the
/// layout it knows, or entries none of which it could read — is an
/// error: nothing is written or deleted, and the file is not stamped, so
/// a build that reads the new layout picks it up.
///
/// A file absent from the scan, or unchanged, does nothing. Absent is not
/// empty: an export requested without this product has no such file.
pub async fn ingest_snapshot<T, F>(
    pool: &SqlitePool,
    scope: &str,
    file: Option<&ScannedFile>,
    parse: F,
) -> Result<SnapshotCounts>
where
    T: datalib_etl::bulk::BulkUpsertable,
    F: FnOnce(&[u8]) -> Result<Vec<T>>,
{
    let Some(f) = file else {
        return Ok(SnapshotCounts::default());
    };
    if crate::fsscan::is_unchanged(&load_cursor(pool, scope).await?, f) {
        return Ok(SnapshotCounts::default());
    }

    let bytes = std::fs::read(&f.path).with_context(|| format!("read {}", f.path.display()))?;
    let rows = parse(&bytes)?;

    let now = datalib_time::IsoOffsetTimestamp::now_local();
    let mut tx = pool
        .begin()
        .await
        .with_context(|| format!("begin {scope} tx"))?;
    datalib_etl::bulk::bulk_upsert_in_tx(&mut tx, &rows, &now).await?;
    let keep: HashSet<String> = rows.iter().map(|r| r.id().to_string()).collect();
    let gone = datalib_etl::prune::prune_scope_in_tx(&mut tx, T::TABLE, &[], &keep).await?;
    datalib_etl::prune::record(T::TABLE, keep.len() + gone.len(), gone.len());
    let counts = SnapshotCounts {
        written: rows.len(),
        removed: gone.len(),
    };
    record_file(&mut tx, scope, f).await?;
    tx.commit()
        .await
        .with_context(|| format!("commit {scope} tx"))?;
    Ok(counts)
}

/// `DELETE FROM ingested_files WHERE scope = ?`. Use from a provider's
/// `reset` path when wiping per-feed state.
pub async fn clear_scope(pool: &SqlitePool, scope: &str) -> Result<()> {
    ensure_schema(pool).await?;
    forget_problems_under(pool, &format!("{FILE_PROBLEM_PREFIX}{scope}:")).await?;
    sqlx::query("DELETE FROM ingested_files WHERE scope = ?")
        .bind(scope)
        .execute(pool)
        .await
        .with_context(|| format!("clear ingested_files scope={scope}"))?;
    Ok(())
}

pub async fn clear_scope_prefix(pool: &SqlitePool, prefix: &str) -> Result<()> {
    ensure_schema(pool).await?;
    forget_problems_under(pool, &format!("{FILE_PROBLEM_PREFIX}{prefix}")).await?;
    sqlx::query("DELETE FROM ingested_files WHERE scope LIKE ?")
        .bind(format!("{prefix}%"))
        .execute(pool)
        .await
        .with_context(|| format!("clear ingested_files scope LIKE {prefix}%"))?;
    Ok(())
}

async fn forget_problems_under(pool: &SqlitePool, key_prefix: &str) -> Result<()> {
    // `INSTR(x, ?) = 1` rather than `LIKE`: `_` in a path is a wildcard to
    // LIKE.
    sqlx::query("DELETE FROM problems WHERE scope_kind = ? AND INSTR(scope_key, ?) = 1")
        .bind(datalib_problems::ScopeKind::Entity.as_str())
        .bind(key_prefix)
        .execute(pool)
        .await
        .with_context(|| format!("clear the file problems under {key_prefix}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fingerprint_cache::FingerprintCache;
    use crate::fsscan;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::path::Path;
    use std::str::FromStr;
    use tempfile::tempdir;

    /// The store forgets every cursor when a table is recreated, and it
    /// finds them by name in `datalib_etl`, which cannot see this crate.
    /// A cursor table missing from that list would survive a schema
    /// change and let the next run skip files the table no longer has.
    #[test]
    fn the_store_forgets_this_cursor_on_a_schema_change() {
        assert!(datalib_etl::doltlite_raw::CURSOR_TABLES.contains(&INGESTED_FILES_TABLE));
    }

    /// A provider store, a private fingerprint cache, and a tree to
    /// scan — so no test touches, or is influenced by, the host's real
    /// cache.
    struct Env {
        _dir: tempfile::TempDir,
        tree: std::path::PathBuf,
        pool: SqlitePool,
        cache: FingerprintCache,
    }

    async fn env() -> Env {
        let dir = tempdir().unwrap();
        let tree = dir.path().join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        let opts = SqliteConnectOptions::from_str(&format!(
            "sqlite://{}",
            dir.path().join("s.sqlite").display()
        ))
        .unwrap()
        .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(opts)
            .await
            .unwrap();
        ensure_schema(&pool).await.unwrap();
        sqlx::query(datalib_etl::doltlite_raw::PROBLEMS_DDL)
            .execute(&pool)
            .await
            .unwrap();
        let cache = FingerprintCache::open(&dir.path().join("fp.sqlite"))
            .await
            .unwrap();
        Env {
            _dir: dir,
            tree,
            pool,
            cache,
        }
    }

    impl Env {
        async fn scan(&self) -> fsscan::Scan {
            fsscan::scan(
                &self.cache,
                &self.tree,
                &fsscan::ScanOptions::default(),
                |_| true,
            )
            .await
            .unwrap()
        }
        fn write(&self, name: &str, bytes: &[u8]) {
            std::fs::write(self.tree.join(name), bytes).unwrap();
        }
    }

    /// A file the cursor has stamped is not offered again.
    #[tokio::test]
    async fn a_stamped_file_is_not_read_again() {
        let e = env().await;
        e.write("a.txt", b"hello");
        let scan = e.scan().await;
        let f = scan.file("a.txt").unwrap();
        record_file_pool(&e.pool, "p/feed", f).await.unwrap();

        let cursor = load_cursor(&e.pool, "p/feed").await.unwrap();
        assert!(fsscan::is_unchanged(&cursor, f));
        assert_eq!(
            e.scan()
                .await
                .changes_since(&cursor)
                .needs_reading()
                .count(),
            0
        );
    }

    async fn file_problems(pool: &SqlitePool) -> Vec<(String, String)> {
        sqlx::query_as("SELECT scope_key, first_seen_at_utc FROM problems ORDER BY scope_key")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    async fn stamp(e: &Env, scope: &str, f: &ScannedFile, unusable: Option<&str>) {
        use datalib_problems::{Outcome, Problem, Reason};
        let problem = unusable.map(|d| {
            (
                Outcome::Dropped,
                Problem::record(Reason::Undeserializable, d),
            )
        });
        let mut tx = e.pool.begin().await.unwrap();
        record_file_with_problem(&mut tx, scope, f, problem)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    /// What a read of a file could not use stands while the file is
    /// stamped — the file is not read again, so nothing could have fixed
    /// it — keeps when it was first seen across a re-read that still
    /// fails, and goes with a clean re-read, with the file, or with a
    /// reset of its scope.
    #[tokio::test]
    async fn a_files_problem_lives_exactly_as_long_as_its_stamp_says() {
        let e = env().await;
        e.write("a.mbox", b"one bad message");
        e.write("b.mbox", b"another");
        let scan = e.scan().await;
        let (a, b) = (scan.file("a.mbox").unwrap(), scan.file("b.mbox").unwrap());

        stamp(&e, "p/feed", a, Some("message 3 would not parse")).await;
        stamp(&e, "p/feed", b, Some("message 1 would not parse")).await;
        let first = file_problems(&e.pool).await;
        assert_eq!(
            first.iter().map(|r| r.0.as_str()).collect::<Vec<_>>(),
            ["file:p/feed:a.mbox", "file:p/feed:b.mbox"]
        );

        stamp(&e, "p/feed", a, Some("message 3 would not parse")).await;
        assert_eq!(
            file_problems(&e.pool).await,
            first,
            "still failing, first seen kept"
        );

        stamp(&e, "p/feed", a, None).await;
        let mut tx = e.pool.begin().await.unwrap();
        forget_file(&mut tx, "p/feed", "b.mbox").await.unwrap();
        tx.commit().await.unwrap();
        assert!(file_problems(&e.pool).await.is_empty());

        stamp(&e, "p/feed", a, Some("message 3 would not parse")).await;
        clear_scope(&e.pool, "p/feed").await.unwrap();
        assert!(file_problems(&e.pool).await.is_empty());
    }

    #[tokio::test]
    async fn scope_namespaces_rows() {
        let e = env().await;
        e.write("a.txt", b"hello");
        let scan = e.scan().await;
        let f = scan.file("a.txt").unwrap();
        record_file_pool(&e.pool, "p/one", f).await.unwrap();
        // A different scope sees nothing for the same file.
        let other = load_cursor(&e.pool, "p/two").await.unwrap();
        assert!(!fsscan::is_unchanged(&other, f));
    }

    #[tokio::test]
    async fn changed_content_is_read_again() {
        let e = env().await;
        e.write("a.txt", b"hello");
        let first = e.scan().await;
        record_file_pool(&e.pool, "p/feed", first.file("a.txt").unwrap())
            .await
            .unwrap();

        e.write("a.txt", b"hello, world");
        let cursor = load_cursor(&e.pool, "p/feed").await.unwrap();
        let second = e.scan().await;
        assert_eq!(second.changes_since(&cursor).needs_reading().count(), 1);
    }

    /// The defect the content hash exists to fix. Under the old `(size, mtime)`
    /// cursor `touch` moved the mtime, the stamp stopped matching, and the whole
    /// file re-ingested though not one byte had changed.
    #[tokio::test]
    async fn touching_a_file_does_not_re_ingest_it() {
        let e = env().await;
        e.write("a.txt", b"hello");
        let first = e.scan().await;
        record_file_pool(&e.pool, "p/feed", first.file("a.txt").unwrap())
            .await
            .unwrap();

        // Same bytes, later mtime — which is all `touch` does.
        std::thread::sleep(std::time::Duration::from_millis(10));
        e.write("a.txt", b"hello");

        let cursor = load_cursor(&e.pool, "p/feed").await.unwrap();
        let second = e.scan().await;
        assert!(
            second.file("a.txt").unwrap().blake3 == first.file("a.txt").unwrap().blake3,
            "same bytes, same hash",
        );
        assert_eq!(
            second.changes_since(&cursor).needs_reading().count(),
            0,
            "a touched but unedited file must not re-ingest",
        );
    }

    /// The boundary this mechanism does **not** cross, pinned so that "content
    /// hash" is never read as "always re-reads". The cache decides whether to
    /// re-hash from Unison's `(mtime, size, inode, dev)` cursor, so an edit
    /// preserving all four hands back the cached hash and the file is skipped.
    #[tokio::test]
    async fn an_edit_preserving_the_whole_stat_is_still_invisible() {
        let e = env().await;
        e.write("a.txt", b"aaaaa");
        let first = e.scan().await;
        record_file_pool(&e.pool, "p/feed", first.file("a.txt").unwrap())
            .await
            .unwrap();

        // Same length, different bytes, mtime put back where it was —
        // an in-place rewrite, so the inode does not move either.
        let p = e.tree.join("a.txt");
        let when = std::fs::metadata(&p).unwrap().modified().unwrap();
        std::fs::write(&p, b"bbbbb").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&p)
            .unwrap()
            .set_modified(when)
            .unwrap();

        let cursor = load_cursor(&e.pool, "p/feed").await.unwrap();
        let second = e.scan().await;
        assert_eq!(
            second.file("a.txt").unwrap().blake3,
            first.file("a.txt").unwrap().blake3,
            "the cache vouched for a stat that did not move",
        );
        assert_eq!(
            second.changes_since(&cursor).needs_reading().count(),
            0,
            "so the edit is not seen",
        );
    }

    #[tokio::test]
    async fn an_outdated_table_is_dropped_not_read() {
        let e = env().await;
        sqlx::query("DROP TABLE ingested_files")
            .execute(&e.pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE ingested_files (scope TEXT NOT NULL, path TEXT NOT NULL,
             size_bytes INTEGER NOT NULL, mtime_ns INTEGER NOT NULL,
             last_finished_at_utc TEXT NOT NULL, PRIMARY KEY (scope, path))",
        )
        .execute(&e.pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO ingested_files VALUES ('p/feed','a.txt',5,1,'t')")
            .execute(&e.pool)
            .await
            .unwrap();

        // Reading the scope must succeed and report nothing stamped,
        // rather than failing on the missing column.
        assert!(load_cursor(&e.pool, "p/feed").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn clear_scope_prefix_drops_all_matching() {
        let e = env().await;
        e.write("a.txt", b"hi");
        let scan = e.scan().await;
        let f = scan.file("a.txt").unwrap();
        for scope in ["google_takeout/maps", "google_takeout/youtube", "other/x"] {
            record_file_pool(&e.pool, scope, f).await.unwrap();
        }
        clear_scope_prefix(&e.pool, "google_takeout/")
            .await
            .unwrap();
        let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM ingested_files")
            .fetch_one(&e.pool)
            .await
            .unwrap();
        assert_eq!(remaining, 1);
        let _: &Path = &e.tree;
    }
}
