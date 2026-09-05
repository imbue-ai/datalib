//! Shared `(scope, path, blake3)` resume cursor.
//!
//! Answers one question for every file-backed provider: **has this file
//! changed since this feed last finished with it?** The answer is a
//! content hash, and the hash comes from the host-wide fingerprint
//! cache in [`crate::fingerprint_cache`] — the same cache, and the same
//! Unison `(mtime, size, inode, dev)` rescan cursor, that `fsindex`,
//! `pdf`, `media` and `signal` use when they scan a whole tree.
//!
//! That sharing is the design. A file's identity should not depend on
//! which caller is asking, so a file hashed by a tree scan is free
//! here, and a file hashed here is free to a later tree scan.
//!
//! Each scope namespaces rows per `(provider, feed)`, so two feeds can
//! claim the same on-disk path without colliding.
//!
//! Surface:
//!
//! - [`INGESTED_FILES_DDL`] — table DDL; splice into the provider's
//!   `full_ddl()`.
//! - [`ensure_schema`] — creates it, and drops a pre-content-hash one.
//! - [`FileFingerprint::of`] — the file's blake3, via the cache.
//! - [`load`] — bulk pre-load of `(canonical_path → blake3)` for a
//!   scope. One round trip per fetch.
//! - [`should_skip`] — true when the stamped hash matches the file's.
//! - [`record_finished`] — UPSERT, called inside the same tx that
//!   flushed the file's last batch.
//! - [`ingest_changed_file`] — all of the above, for the common case.
//!
//! **Why content and not `(size, mtime)`.** It used to be the stat
//! pair, on the reasoning that hashing was too expensive to do every
//! run. The cache removed that cost. What the stat pair got wrong was
//! the *false re-ingest*: touching a file — `rsync` without `-t`, a
//! restore from backup, re-downloading the same export — re-read and
//! re-parsed the whole thing though not one byte had moved.
//!
//! Be clear about what this does **not** fix. The cache decides
//! whether to re-hash from Unison's `(mtime, size, inode, dev)`
//! cursor, so an edit preserving all four still returns the cached
//! hash and is still skipped. That was equally true before; the gain
//! is that the assumption now lives in one place, shared with every
//! tree scan, instead of being restated per provider. Pinned by
//! `an_edit_preserving_the_whole_stat_is_still_invisible`. A caller
//! that cannot tolerate it wants a forced re-hash, not a different
//! cursor.
//!
//! The path is still part of the key, so a *rename* re-ingests. The
//! hash to recognise a moved file is now sitting right here, but
//! whether two paths with identical bytes are one entity or two is a
//! per-feed question — some feeds derive ids from the filename — so
//! that is deliberately left to the caller rather than assumed.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sqlx::{Sqlite, SqlitePool, Transaction};

use crate::fingerprint_cache::FingerprintCache;

/// Shared resume-cursor table. One row per `(scope, canonical_path)`.
///
/// Scope names should be `"<provider>/<feed>"` (e.g.
/// `"google_takeout/maps_reviews"`); collisions across providers are
/// the caller's responsibility to avoid.
pub const INGESTED_FILES_DDL: &str = "CREATE TABLE IF NOT EXISTS ingested_files (
    scope TEXT NOT NULL,
    path TEXT NOT NULL,
    blake3 TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    last_finished_at TEXT NOT NULL,
    PRIMARY KEY (scope, path)
)";

/// Create the table, dropping one written before the checkpoint became
/// content-based.
///
/// A pre-blake3 store has `mtime_ns` where `blake3` now goes, and no
/// migration can invent hashes it never recorded. Dropping is the
/// honest move and a cheap one: this table is a cursor, so losing it
/// costs one re-ingest and never data.
pub async fn ensure_schema(pool: &SqlitePool) -> Result<()> {
    let cols: Vec<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info('ingested_files')")
            .fetch_all(pool)
            .await
            .context("inspect ingested_files")?;
    if !cols.is_empty() && !cols.iter().any(|c| c == "blake3") {
        sqlx::query("DROP TABLE ingested_files")
            .execute(pool)
            .await
            .context("drop pre-blake3 ingested_files")?;
        tracing::info!(
            event = "ingested_files_reset",
            "checkpoint predates content hashing; dropped so the next run re-reads",
        );
    }
    sqlx::query(INGESTED_FILES_DDL)
        .execute(pool)
        .await
        .context("create ingested_files")?;
    Ok(())
}

/// What a file was, last time this feed finished it: the canonical
/// absolute path used as the cursor PK, the blake3 of its bytes, and
/// its size (carried for reporting, never for the decision).
#[derive(Debug, Clone)]
pub struct FileFingerprint {
    pub canonical: String,
    pub blake3: String,
    pub size_bytes: u64,
}

impl FileFingerprint {
    /// The file's current fingerprint, via the shared cache — so an
    /// unchanged file costs a `stat`, not a re-read.
    ///
    /// `None` when the path is not a readable file, which is what an
    /// absent export file looks like.
    pub async fn of(cache: &FingerprintCache, path: &Path) -> Result<Option<Self>> {
        let Some(fh) = crate::fsscan::fingerprint_file(cache, path).await? else {
            return Ok(None);
        };
        Ok(Some(Self {
            canonical: fh.canonical.clone(),
            blake3: fh.hex(),
            size_bytes: fh.size as u64,
        }))
    }
}

/// Pre-load every stamped hash under `scope`, keyed by canonical path.
/// One HashMap hit per file vs N round trips.
pub async fn load(pool: &SqlitePool, scope: &str) -> Result<HashMap<String, String>> {
    ensure_schema(pool).await?;
    let rows = sqlx::query_as::<_, (String, String)>(
        "SELECT path, blake3 FROM ingested_files WHERE scope = ?",
    )
    .bind(scope)
    .fetch_all(pool)
    .await
    .with_context(|| format!("load ingested_files scope={scope}"))?;
    Ok(rows.into_iter().collect())
}

/// True iff the stamped row's hash matches the file's current one.
/// Looked up against the pre-loaded map from [`load`].
pub fn should_skip(stamped: &HashMap<String, String>, fp: &FileFingerprint) -> bool {
    stamped.get(&fp.canonical).is_some_and(|h| *h == fp.blake3)
}

/// Stamp `(scope, fp.canonical)` with the current fingerprint. Runs
/// inside the caller's transaction so a crash after the file's last
/// batch but before the commit leaves no stamped row for partially-
/// ingested content.
pub async fn record_finished(
    tx: &mut Transaction<'_, Sqlite>,
    scope: &str,
    fp: &FileFingerprint,
) -> Result<()> {
    let now = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    sqlx::query(
        "INSERT INTO ingested_files (scope, path, blake3, size_bytes, last_finished_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(scope, path) DO UPDATE SET
            blake3 = excluded.blake3,
            size_bytes = excluded.size_bytes,
            last_finished_at = excluded.last_finished_at",
    )
    .bind(scope)
    .bind(&fp.canonical)
    .bind(&fp.blake3)
    .bind(fp.size_bytes as i64)
    .bind(&now)
    .execute(&mut **tx)
    .await
    .with_context(|| format!("upsert ingested_files {scope}={}", fp.canonical))?;
    Ok(())
}

/// Ingest one file, but only if its contents have changed since
/// `scope` last finished with it.
///
/// Every single-file feed repeated the same dozen lines around its
/// parser: does the file exist, has it changed, read it, parse it,
/// open a transaction, write the rows, stamp the checkpoint, commit.
/// Only the path, the scope and the parser ever differed. This is that
/// dozen lines, once.
///
/// The ordering is the part worth centralising: the rows and the
/// checkpoint land in **one** transaction, so a crash between them
/// cannot leave a stamp claiming content that never arrived. Getting
/// that wrong in one feed out of nine would be invisible until it
/// mattered.
///
/// Returns the number of rows written — `0` when the file is absent or
/// unchanged, which is what a caller reports as "nothing to do".
///
/// A parse that yields no rows still stamps: the file was read and
/// understood to contain nothing, and re-reading it on every future run
/// would be the "retry forever" shape rather than a fresh look. If its
/// bytes change, the hash changes and it is read again.
pub async fn ingest_changed_file<T, F>(
    cache: &FingerprintCache,
    pool: &SqlitePool,
    scope: &str,
    path: &Path,
    parse: F,
) -> Result<usize>
where
    T: crate::bulk::BulkUpsertable,
    F: FnOnce(&[u8]) -> Result<Vec<T>>,
{
    let Some(fp) = FileFingerprint::of(cache, path).await? else {
        return Ok(0);
    };
    let stamped = load(pool, scope).await?;
    if should_skip(&stamped, &fp) {
        return Ok(0);
    }

    let bytes = std::fs::read(path).with_context(|| format!("read {}", path.display()))?;
    let rows = parse(&bytes)?;
    let n = rows.len();

    let now = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    let mut tx = pool
        .begin()
        .await
        .with_context(|| format!("begin {scope} tx"))?;
    crate::bulk::bulk_upsert_in_tx(&mut tx, &rows, &now).await?;
    record_finished(&mut tx, scope, &fp).await?;
    tx.commit()
        .await
        .with_context(|| format!("commit {scope} tx"))?;
    Ok(n)
}

/// One-shot convenience for callers that don't already own a tx.
pub async fn record_finished_pool(
    pool: &SqlitePool,
    scope: &str,
    fp: &FileFingerprint,
) -> Result<()> {
    let mut tx = pool.begin().await.context("begin record_finished tx")?;
    record_finished(&mut tx, scope, fp).await?;
    tx.commit().await.context("commit record_finished tx")?;
    Ok(())
}

/// `DELETE FROM ingested_files WHERE scope = ?`. Use from a
/// provider's `reset` path when wiping per-feed state.
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

/// Convenience for callers that want to bundle the path + fingerprint
/// once and pass both into the walker. Mirrors mbox's `MboxJob`.
#[derive(Debug, Clone)]
pub struct CheckpointedFile {
    pub path: PathBuf,
    pub fingerprint: FileFingerprint,
}

impl CheckpointedFile {
    /// `None` when the path is not a readable file.
    pub async fn of(cache: &FingerprintCache, path: &Path) -> Result<Option<Self>> {
        Ok(FileFingerprint::of(cache, path)
            .await?
            .map(|fingerprint| Self {
                path: path.to_path_buf(),
                fingerprint,
            }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;
    use tempfile::tempdir;

    /// A provider store and a private fingerprint cache, so a test
    /// never touches the host's real one.
    async fn tmp_env() -> (tempfile::TempDir, SqlitePool, FingerprintCache) {
        let d = tempdir().unwrap();
        let path = d.path().join("test.sqlite");
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .unwrap()
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        ensure_schema(&pool).await.unwrap();
        let cache = FingerprintCache::open(&d.path().join("cache.sqlite"))
            .await
            .unwrap();
        (d, pool, cache)
    }

    async fn fp_of(cache: &FingerprintCache, path: &Path) -> FileFingerprint {
        FileFingerprint::of(cache, path).await.unwrap().unwrap()
    }

    #[tokio::test]
    async fn record_then_should_skip_matches() {
        let (d, pool, cache) = tmp_env().await;
        let f = d.path().join("a.txt");
        std::fs::write(&f, b"hello").unwrap();
        let fp = fp_of(&cache, &f).await;
        record_finished_pool(&pool, "p/feed", &fp).await.unwrap();
        let stamped = load(&pool, "p/feed").await.unwrap();
        assert!(should_skip(&stamped, &fp));
    }

    #[tokio::test]
    async fn scope_namespaces_rows() {
        let (d, pool, cache) = tmp_env().await;
        let f = d.path().join("a.txt");
        std::fs::write(&f, b"hello").unwrap();
        let fp = fp_of(&cache, &f).await;
        record_finished_pool(&pool, "p/one", &fp).await.unwrap();
        // Different scope sees nothing for the same path.
        let other = load(&pool, "p/two").await.unwrap();
        assert!(!should_skip(&other, &fp));
    }

    #[tokio::test]
    async fn changed_content_means_no_skip() {
        let (d, pool, cache) = tmp_env().await;
        let f = d.path().join("a.txt");
        std::fs::write(&f, b"hello").unwrap();
        let fp1 = fp_of(&cache, &f).await;
        record_finished_pool(&pool, "p/feed", &fp1).await.unwrap();
        std::fs::write(&f, b"hello, world").unwrap();
        let fp2 = fp_of(&cache, &f).await;
        let stamped = load(&pool, "p/feed").await.unwrap();
        assert!(!should_skip(&stamped, &fp2));
    }

    /// The defect the content hash exists to fix.
    ///
    /// Under the old `(size, mtime)` cursor this asserted the opposite
    /// by accident: `touch` moved the mtime, the stamp stopped
    /// matching, and the whole file re-ingested even though not one
    /// byte had changed. `rsync` without `-t`, a restore from backup
    /// and re-downloading the same export all land here.
    #[tokio::test]
    async fn touching_a_file_does_not_re_ingest_it() {
        let (d, pool, cache) = tmp_env().await;
        let f = d.path().join("a.txt");
        std::fs::write(&f, b"hello").unwrap();
        let fp1 = fp_of(&cache, &f).await;
        record_finished_pool(&pool, "p/feed", &fp1).await.unwrap();

        // Same bytes, later mtime — which is all `touch` does.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(&f, b"hello").unwrap();

        let fp2 = fp_of(&cache, &f).await;
        assert_eq!(fp1.blake3, fp2.blake3, "same bytes, same hash");
        let stamped = load(&pool, "p/feed").await.unwrap();
        assert!(
            should_skip(&stamped, &fp2),
            "a touched but unedited file must not re-ingest",
        );
    }

    /// The boundary this mechanism does **not** cross, pinned so that
    /// "content hash" is never read as "always re-reads".
    ///
    /// The cache decides whether to re-hash from Unison's
    /// `(mtime, size, inode, dev)` cursor. An edit preserving all four
    /// hands back the cached hash, and the file is skipped. The old
    /// `(size, mtime)` cursor was blind to exactly the same edit, so
    /// nothing regressed here — but a reader who assumes hashing
    /// closed this hole would be wrong, which is why it is a test and
    /// not a comment.
    #[tokio::test]
    async fn an_edit_preserving_the_whole_stat_is_still_invisible() {
        let (d, pool, cache) = tmp_env().await;
        let f = d.path().join("a.txt");
        std::fs::write(&f, b"aaaaa").unwrap();
        let fp1 = fp_of(&cache, &f).await;
        record_finished_pool(&pool, "p/feed", &fp1).await.unwrap();

        // Same length, different bytes, mtime put back where it was —
        // an in-place rewrite, so the inode does not move either.
        let when = std::fs::metadata(&f).unwrap().modified().unwrap();
        std::fs::write(&f, b"bbbbb").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&f)
            .unwrap()
            .set_modified(when)
            .unwrap();

        let fp2 = fp_of(&cache, &f).await;
        assert_eq!(
            fp1.blake3, fp2.blake3,
            "the cache vouched for a stat that did not move",
        );
        let stamped = load(&pool, "p/feed").await.unwrap();
        assert!(should_skip(&stamped, &fp2), "so the edit is not seen");
    }

    #[tokio::test]
    async fn a_pre_blake3_table_is_dropped_not_read() {
        let (d, pool, _cache) = tmp_env().await;
        sqlx::query("DROP TABLE ingested_files")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE ingested_files (scope TEXT NOT NULL, path TEXT NOT NULL,
             size_bytes INTEGER NOT NULL, mtime_ns INTEGER NOT NULL,
             last_finished_at TEXT NOT NULL, PRIMARY KEY (scope, path))",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO ingested_files VALUES ('p/feed','/a.txt',5,1,'t')")
            .execute(&pool)
            .await
            .unwrap();

        // Reading the scope must succeed and report nothing stamped,
        // rather than failing on the missing column.
        let stamped = load(&pool, "p/feed").await.unwrap();
        assert!(stamped.is_empty());
        let _ = d;
    }

    #[tokio::test]
    async fn clear_scope_prefix_drops_all_matching() {
        let (d, pool, cache) = tmp_env().await;
        let f = d.path().join("a.txt");
        std::fs::write(&f, b"hi").unwrap();
        let fp = fp_of(&cache, &f).await;
        record_finished_pool(&pool, "google_takeout/maps", &fp)
            .await
            .unwrap();
        record_finished_pool(&pool, "google_takeout/youtube", &fp)
            .await
            .unwrap();
        record_finished_pool(&pool, "other_provider/x", &fp)
            .await
            .unwrap();
        clear_scope_prefix(&pool, "google_takeout/").await.unwrap();
        let remaining: i64 = sqlx::query_scalar("SELECT count(*) FROM ingested_files")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(remaining, 1);
    }
}
