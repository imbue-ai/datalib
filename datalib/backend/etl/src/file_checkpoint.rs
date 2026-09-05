//! Where a file-backed source got to: its resume cursor, per feed.
//!
//! One question, for every provider that reads files off disk: **which
//! files have changed since this feed last finished with them?**
//!
//! The answer is a content hash, and it comes from
//! [`crate::fsscan`] — one scan of the source's root, hashing only
//! what the host-wide fingerprint cache cannot vouch for. This module
//! is only the persistent half: the cursor a feed stores so that "since
//! *I* last looked" is well-posed, since the cache itself is shared and
//! another consumer's scan moves it.
//!
//! The shape every caller wants:
//!
//! ```text
//!     let scan    = fsscan::scan(cache, root, opts, accept).await?;
//!     let changes = scan.changes_since(&load_cursor(pool, SCOPE).await?);
//!     for f in changes.needs_reading() { … ; record_file(&mut tx, SCOPE, f).await?; }
//! ```
//!
//! Each scope namespaces rows per `(provider, feed)`, so two feeds can
//! claim the same file without colliding.
//!
//! **Why content and not `(size, mtime)`.** It was the stat pair, on
//! the reasoning that hashing every run was too expensive. The cache
//! removed that cost. What the stat pair got wrong was the *false
//! re-ingest*: touching a file — `rsync` without `-t`, a restore from
//! backup, re-downloading the same export — re-read and re-parsed the
//! whole thing though not one byte had moved.
//!
//! Be clear about what this does **not** fix, because "content hash"
//! invites the wrong assumption: the cache still decides whether to
//! re-hash from Unison's `(mtime, size, inode, dev)` cursor, so an edit
//! preserving all four is still invisible. That was equally true
//! before. The gain is that the assumption lives in one place, shared
//! with every scan, instead of once per provider — and
//! `an_edit_preserving_the_whole_stat_is_still_invisible` pins it so
//! nobody reads more into the change than it delivers.

use anyhow::{Context, Result};
use sqlx::{Sqlite, SqlitePool, Transaction};

use crate::fsscan::{FileScanCursor, ScannedFile};

/// The cursor table. One row per `(scope, root-relative path)`.
///
/// Scope names should be `"<provider>/<feed>"` (e.g.
/// `"google_takeout/maps_reviews"`); collisions across providers are
/// the caller's responsibility to avoid.
pub const INGESTED_FILES_DDL: &str = "CREATE TABLE IF NOT EXISTS ingested_files (
    scope TEXT NOT NULL,
    rel_path TEXT NOT NULL,
    blake3 TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    last_finished_at TEXT NOT NULL,
    PRIMARY KEY (scope, rel_path)
)";

/// Create the table, dropping one written to an older shape.
///
/// A store from before the cursor became content-based has `mtime_ns`
/// where `blake3` now goes, and no migration can invent hashes it never
/// recorded. Dropping is the honest move and a cheap one: this table is
/// a cursor, so losing it costs one re-ingest and never data.
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
/// Per file, not per run, and deliberately: a crash partway through a
/// directory keeps the files that did land and re-reads only the rest.
/// Callers run this in the same transaction that wrote the file's rows,
/// so a crash between the two cannot leave a stamp claiming content
/// that never arrived.
pub async fn record_file(
    tx: &mut Transaction<'_, Sqlite>,
    scope: &str,
    file: &ScannedFile,
) -> Result<()> {
    let now = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    sqlx::query(
        "INSERT INTO ingested_files (scope, rel_path, blake3, size_bytes, last_finished_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(scope, rel_path) DO UPDATE SET
            blake3 = excluded.blake3,
            size_bytes = excluded.size_bytes,
            last_finished_at = excluded.last_finished_at",
    )
    .bind(scope)
    .bind(&file.rel)
    .bind(crate::fsscan::hex(&file.blake3))
    .bind(file.size)
    .bind(&now)
    .execute(&mut **tx)
    .await
    .with_context(|| format!("upsert ingested_files {scope}={}", file.rel))?;
    Ok(())
}

/// [`record_file`] for callers that don't already own a transaction.
pub async fn record_file_pool(pool: &SqlitePool, scope: &str, file: &ScannedFile) -> Result<()> {
    let mut tx = pool.begin().await.context("begin record_file tx")?;
    record_file(&mut tx, scope, file).await?;
    tx.commit().await.context("commit record_file tx")?;
    Ok(())
}

/// Ingest one already-scanned file, if its contents have changed since
/// `scope` last finished with it.
///
/// Every single-file feed repeated the same dozen lines around its
/// parser: is the file there, has it changed, read it, parse it, open a
/// transaction, write the rows, stamp the cursor, commit. Only the
/// scope and the parser ever differed. This is that dozen lines, once.
///
/// It takes a [`ScannedFile`] rather than a path because by the time a
/// feed runs, its provider has already scanned the export root — the
/// file's existence and its hash are known, and re-`stat`ing it here
/// would be asking a question that has been answered.
///
/// Returns the number of rows written — `0` when the file is absent
/// from the scan or unchanged, which a caller reports as "nothing to
/// do".
///
/// A parse that yields no rows still stamps: the file was read and
/// understood to contain nothing, and re-reading it every run would be
/// the "retry forever" shape. If its bytes change, so does the hash.
pub async fn ingest_changed<T, F>(
    pool: &SqlitePool,
    scope: &str,
    file: Option<&ScannedFile>,
    parse: F,
) -> Result<usize>
where
    T: crate::bulk::BulkUpsertable,
    F: FnOnce(&[u8]) -> Result<Vec<T>>,
{
    let Some(f) = file else {
        return Ok(0);
    };
    if crate::fsscan::is_unchanged(&load_cursor(pool, scope).await?, f) {
        return Ok(0);
    }

    let bytes = std::fs::read(&f.path).with_context(|| format!("read {}", f.path.display()))?;
    let rows = parse(&bytes)?;
    let n = rows.len();

    let now = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    let mut tx = pool
        .begin()
        .await
        .with_context(|| format!("begin {scope} tx"))?;
    crate::bulk::bulk_upsert_in_tx(&mut tx, &rows, &now).await?;
    record_file(&mut tx, scope, f).await?;
    tx.commit()
        .await
        .with_context(|| format!("commit {scope} tx"))?;
    Ok(n)
}

/// `DELETE FROM ingested_files WHERE scope = ?`. Use from a provider's
/// `reset` path when wiping per-feed state.
pub async fn clear_scope(pool: &SqlitePool, scope: &str) -> Result<()> {
    ensure_schema(pool).await?;
    sqlx::query("DELETE FROM ingested_files WHERE scope = ?")
        .bind(scope)
        .execute(pool)
        .await
        .with_context(|| format!("clear ingested_files scope={scope}"))?;
    Ok(())
}

/// `DELETE FROM ingested_files WHERE scope LIKE ?`. Use from a
/// provider's `reset` when wiping every scope it owns
/// (e.g. `"google_takeout/%"`).
pub async fn clear_scope_prefix(pool: &SqlitePool, prefix: &str) -> Result<()> {
    ensure_schema(pool).await?;
    sqlx::query("DELETE FROM ingested_files WHERE scope LIKE ?")
        .bind(format!("{prefix}%"))
        .execute(pool)
        .await
        .with_context(|| format!("clear ingested_files scope LIKE {prefix}%"))?;
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
            .connect_with(opts)
            .await
            .unwrap();
        ensure_schema(&pool).await.unwrap();
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

    /// The defect the content hash exists to fix.
    ///
    /// Under the old `(size, mtime)` cursor this went the other way:
    /// `touch` moved the mtime, the stamp stopped matching, and the
    /// whole file re-ingested though not one byte had changed. `rsync`
    /// without `-t`, a restore from backup, and re-downloading the same
    /// export all land here.
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

    /// The boundary this mechanism does **not** cross, pinned so that
    /// "content hash" is never read as "always re-reads".
    ///
    /// The cache decides whether to re-hash from Unison's
    /// `(mtime, size, inode, dev)` cursor. An edit preserving all four
    /// hands back the cached hash, and the file is skipped. The old
    /// `(size, mtime)` cursor was blind to the same edit, so nothing
    /// regressed — but a reader who assumes hashing closed this hole
    /// would be wrong, which is why this is a test and not a comment.
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
             last_finished_at TEXT NOT NULL, PRIMARY KEY (scope, path))",
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
