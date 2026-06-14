//! Shared `(scope, path, size_bytes, mtime_ns)` resume cursor.
//!
//! Mbox's per-file checkpoint pattern, lifted into a single shared
//! `ingested_files` table that any provider can use. Each scope
//! namespaces rows per `(provider, feed)` so two feeds can claim the
//! same on-disk path without colliding.
//!
//! Surface:
//!
//! - [`INGESTED_FILES_DDL`] — table DDL; splice into the provider's
//!   `full_ddl()`.
//! - [`FileFingerprint::of`] — one `stat`; returns `(size_bytes,
//!   mtime_ns)` plus the canonicalized path string used as the PK.
//! - [`load`] — bulk pre-load of `(canonical_path → (size, mtime))`
//!   for a scope. Cheap; one round trip per fetch.
//! - [`should_skip`] — true when the stamped row matches the
//!   current fingerprint.
//! - [`record_finished`] — UPSERT, called inside the same tx that
//!   flushed the file's last batch.
//!
//! Why `(size, mtime)` not content hash: cheap to check, sufficient
//! for export-shaped data ("download a new export, point me at it"),
//! consistent with what mbox already does. Path is part of the
//! cursor key, so a rename means re-ingest — the safe default.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use sqlx::{Sqlite, SqlitePool, Transaction};

/// Shared resume-cursor table. One row per `(scope, canonical_path)`.
///
/// Scope names should be `"<provider>/<feed>"` (e.g.
/// `"google_takeout/maps_reviews"`); collisions across providers are
/// the caller's responsibility to avoid.
pub const INGESTED_FILES_DDL: &str = "CREATE TABLE IF NOT EXISTS ingested_files (
    scope TEXT NOT NULL,
    path TEXT NOT NULL,
    size_bytes INTEGER NOT NULL,
    mtime_ns INTEGER NOT NULL,
    last_finished_at TEXT NOT NULL,
    PRIMARY KEY (scope, path)
)";

/// `(size, mtime)` snapshot of an on-disk file, plus the canonical
/// absolute path used as the cursor PK. Built once at scheduling
/// time so relative-vs-absolute spellings collapse to the same row
/// across runs.
#[derive(Debug, Clone)]
pub struct FileFingerprint {
    pub canonical: String,
    pub size_bytes: u64,
    pub mtime_ns: i64,
}

impl FileFingerprint {
    /// One `stat`. Returns the fingerprint + canonical path string
    /// for the file at `path`.
    pub fn of(path: &Path) -> Result<Self> {
        let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
        let mtime = meta
            .modified()
            .with_context(|| format!("mtime {}", path.display()))?;
        let mtime_ns = match mtime.duration_since(UNIX_EPOCH) {
            Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
            // Pre-1970 mtime is exotic enough that we treat it as
            // "never matches" rather than panic.
            Err(_) => i64::MIN,
        };
        let canonical = std::fs::canonicalize(path)
            .with_context(|| format!("canonicalize {}", path.display()))?
            .to_string_lossy()
            .into_owned();
        Ok(Self {
            canonical,
            size_bytes: meta.len(),
            mtime_ns,
        })
    }
}

/// Pre-load every stamped fingerprint under `scope`, keyed by the
/// canonical path. One HashMap hit per file vs N round trips.
pub async fn load(pool: &SqlitePool, scope: &str) -> Result<HashMap<String, (u64, i64)>> {
    let rows = sqlx::query_as::<_, (String, i64, i64)>(
        "SELECT path, size_bytes, mtime_ns FROM ingested_files WHERE scope = ?",
    )
    .bind(scope)
    .fetch_all(pool)
    .await
    .with_context(|| format!("load ingested_files scope={scope}"))?;
    Ok(rows
        .into_iter()
        .map(|(p, sz, mt)| (p, (sz as u64, mt)))
        .collect())
}

/// True iff the stamped row's `(size, mtime)` matches the current
/// fingerprint. Looked up against the pre-loaded map from [`load`].
pub fn should_skip(stamped: &HashMap<String, (u64, i64)>, fp: &FileFingerprint) -> bool {
    stamped
        .get(&fp.canonical)
        .is_some_and(|(sz, mt)| *sz == fp.size_bytes && *mt == fp.mtime_ns)
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
    let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    sqlx::query(
        "INSERT INTO ingested_files (scope, path, size_bytes, mtime_ns, last_finished_at)
         VALUES (?, ?, ?, ?, ?)
         ON CONFLICT(scope, path) DO UPDATE SET
            size_bytes = excluded.size_bytes,
            mtime_ns = excluded.mtime_ns,
            last_finished_at = excluded.last_finished_at",
    )
    .bind(scope)
    .bind(&fp.canonical)
    .bind(fp.size_bytes as i64)
    .bind(fp.mtime_ns)
    .bind(&now)
    .execute(&mut **tx)
    .await
    .with_context(|| format!("upsert ingested_files {scope}={}", fp.canonical))?;
    Ok(())
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
    pub fn of(path: &Path) -> Result<Self> {
        Ok(Self {
            path: path.to_path_buf(),
            fingerprint: FileFingerprint::of(path)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;
    use tempfile::tempdir;

    async fn tmp_pool() -> (tempfile::TempDir, SqlitePool) {
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
        sqlx::query(INGESTED_FILES_DDL)
            .execute(&pool)
            .await
            .unwrap();
        (d, pool)
    }

    #[tokio::test]
    async fn record_then_should_skip_matches() {
        let (d, pool) = tmp_pool().await;
        let f = d.path().join("a.txt");
        std::fs::write(&f, b"hello").unwrap();
        let fp = FileFingerprint::of(&f).unwrap();
        record_finished_pool(&pool, "p/feed", &fp).await.unwrap();
        let stamped = load(&pool, "p/feed").await.unwrap();
        assert!(should_skip(&stamped, &fp));
    }

    #[tokio::test]
    async fn scope_namespaces_rows() {
        let (d, pool) = tmp_pool().await;
        let f = d.path().join("a.txt");
        std::fs::write(&f, b"hello").unwrap();
        let fp = FileFingerprint::of(&f).unwrap();
        record_finished_pool(&pool, "p/one", &fp).await.unwrap();
        // Different scope sees nothing for the same path.
        let other = load(&pool, "p/two").await.unwrap();
        assert!(!should_skip(&other, &fp));
    }

    #[tokio::test]
    async fn changed_size_means_no_skip() {
        let (d, pool) = tmp_pool().await;
        let f = d.path().join("a.txt");
        std::fs::write(&f, b"hello").unwrap();
        let fp1 = FileFingerprint::of(&f).unwrap();
        record_finished_pool(&pool, "p/feed", &fp1).await.unwrap();
        std::fs::write(&f, b"hello, world").unwrap();
        let fp2 = FileFingerprint::of(&f).unwrap();
        let stamped = load(&pool, "p/feed").await.unwrap();
        assert!(!should_skip(&stamped, &fp2));
    }

    #[tokio::test]
    async fn clear_scope_prefix_drops_all_matching() {
        let (d, pool) = tmp_pool().await;
        let f = d.path().join("a.txt");
        std::fs::write(&f, b"hi").unwrap();
        let fp = FileFingerprint::of(&f).unwrap();
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
