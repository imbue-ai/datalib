//! Doltlite-backed raw store for the `fsindex` provider.
//!
//! Mirrors the shape of [`frankweiler_etl_contacts::extract::db`]:
//! `open` runs the full DDL via [`dr::open`], `reset` truncates
//! data tables, and writes go through
//! [`frankweiler_etl::bulk::bulk_upsert_in_tx`] which auto-stamps each
//! table's `_bookkeeping` sidecar.
//!
//! Branch handling: dolt is single-active-branch per connection, and
//! the sql-surface for "create-if-missing" differs across versions.
//! [`Self::checkout_branch`] tries `DOLT_CHECKOUT(branch)` first and
//! falls back to `DOLT_CHECKOUT('-b', branch)`.
//! FIXME(dolt-branch-untested-at-scale): branch ops are not yet
//! bench-verified at scale; the fallback is a pragmatic guess.
//!
//! See [`super::schema_raw`] for the table shapes and
//! [`EXTRACT.md`](../../EXTRACT.md) §"Multi-root via doltlite branches"
//! for why the orchestrator may checkout a non-`main` branch before
//! the scan.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use futures::TryStreamExt;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use frankweiler_etl::bulk::bulk_upsert_in_tx;
use frankweiler_etl::doltlite_raw as dr;

use super::schema_raw::{full_ddl, FileRow, FileStatsRow, ScanMetaRow, StampKind, DATA_TABLES};

/// The Unison fast-rescan cache, loaded once before truncate-and-rebuild.
/// Keyed by root-relative path. Only `kind='file'` rows are loaded —
/// dirs and symlinks always recompute their hash, so they never consult
/// this cache (see `stamp::decide` callers in walker.rs).
pub struct PrevCache {
    /// Prior `(mtime, size, inode, dev, stamp_kind)` per file path.
    pub stats: HashMap<String, FileStatsRow>,
    /// Prior blake3 hex per file path, reused when the stat triple matches.
    pub blake3s: HashMap<String, String>,
}

#[derive(Clone, Debug)]
pub struct RawDb {
    pool: SqlitePool,
}

impl RawDb {
    pub async fn open(db_path: &Path) -> Result<Self> {
        let owned = full_ddl();
        let slices: Vec<&str> = owned.iter().map(String::as_str).collect();
        let pool = dr::open(db_path, &slices).await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Truncate every data table + sidecar so the next walk re-writes
    /// from scratch. Whole-table bookkeeping (`sync_runs`) is
    /// preserved per the framework rule in
    /// [`dr::truncate_data_tables`].
    pub async fn reset(&self) -> Result<()> {
        dr::truncate_data_tables(&self.pool, DATA_TABLES).await
    }

    /// Switch the open connection's active branch, creating it if it
    /// doesn't exist. See module docs for the FIXME on fallback.
    pub async fn checkout_branch(&self, branch: &str) -> Result<()> {
        let try_existing = sqlx::query("CALL DOLT_CHECKOUT(?)")
            .bind(branch)
            .execute(&self.pool)
            .await;
        match try_existing {
            Ok(_) => Ok(()),
            Err(_) => {
                sqlx::query("CALL DOLT_CHECKOUT('-b', ?)")
                    .bind(branch)
                    .execute(&self.pool)
                    .await
                    .with_context(|| format!("dolt checkout -b {branch}"))?;
                Ok(())
            }
        }
    }

    /// Load the Unison fast-rescan cache in ONE streamed pass.
    ///
    /// A single `file_stats JOIN files` query (filtered to
    /// `kind='file'`, the only rows the rescan compare consults)
    /// feeds both maps. We `fetch` a stream and consume by **column
    /// index**, not by name: `fetch_all` materializes every row into
    /// an owned `Vec<SqliteRow>` first, and `try_get(name)` re-resolves
    /// the column name per call — at a million rows those two costs
    /// dominate (observed: minutes vs the engine's ~2 s for the same
    /// query under the `doltlite` CLI). Streaming + positional access
    /// keeps it linear and allocation-light.
    pub async fn load_prev_cache(&self) -> Result<PrevCache> {
        let mut stats: HashMap<String, FileStatsRow> = HashMap::new();
        let mut blake3s: HashMap<String, String> = HashMap::new();
        let mut rows = sqlx::query(
            "SELECT fs.id, fs.mtime_ns, fs.size, fs.stamp_kind, fs.inode, fs.dev, fs.ctime_ns, f.blake3 \
             FROM file_stats fs JOIN files f ON f.id = fs.id WHERE f.kind = 'file'",
        )
        .fetch(&self.pool);
        while let Some(r) = rows.try_next().await.context("stream prev cache")? {
            let id: String = r.try_get(0).context("read id")?;
            let mtime_ns: i64 = r.try_get(1).unwrap_or(0);
            let size: i64 = r.try_get(2).unwrap_or(0);
            let stamp_str: String = r.try_get(3).unwrap_or_else(|_| "rescan".to_string());
            let inode: Option<i64> = r.try_get(4).ok();
            let dev: Option<i64> = r.try_get(5).ok();
            let ctime_ns: Option<i64> = r.try_get(6).ok();
            let blake3: Option<String> = r.try_get(7).ok();
            let stamp_kind = match stamp_str.as_str() {
                "inode" => StampKind::Inode,
                "nostamp" => StampKind::NoStamp,
                _ => StampKind::Rescan,
            };
            if let Some(b) = blake3 {
                blake3s.insert(id.clone(), b);
            }
            stats.insert(
                id.clone(),
                FileStatsRow {
                    id,
                    mtime_ns,
                    size,
                    stamp_kind,
                    inode,
                    dev,
                    ctime_ns,
                },
            );
        }
        Ok(PrevCache { stats, blake3s })
    }

    /// Producer-consumer write path: one batch of file+stat rows
    /// (matched 1:1 by `id`) lands in a SINGLE transaction covering
    /// both `files` and `file_stats` (plus their auto-stamped
    /// `_bookkeeping` sidecars). One commit per batch, not two —
    /// doltlite charges a prolly-tree manifest mutation per commit
    /// and accumulates immutable chunk novelty per commit (reclaimed
    /// only by [`Self::gc`]), so halving the commit count directly
    /// halves transient write amplification during a large scan.
    /// Returns the commit wall time so the orchestrator can attribute
    /// time to the write phase.
    pub async fn write_batch(
        &self,
        files: &[FileRow],
        stats: &[FileStatsRow],
        now: &str,
    ) -> Result<std::time::Duration> {
        let started = std::time::Instant::now();
        if files.is_empty() && stats.is_empty() {
            return Ok(started.elapsed());
        }
        let mut tx = self.pool.begin().await.context("begin batch tx")?;
        if !files.is_empty() {
            bulk_upsert_in_tx(&mut tx, files, now).await?;
        }
        if !stats.is_empty() {
            bulk_upsert_in_tx(&mut tx, stats, now).await?;
        }
        tx.commit().await.context("commit batch tx")?;
        Ok(started.elapsed())
    }

    /// Compact the doltlite chunk store via `dolt_gc()`, reclaiming the
    /// immutable-chunk novelty accumulated across the scan's per-batch
    /// commits. Without this a large scan's on-disk size is dominated
    /// by write amplification (observed ~7 KB/row across hundreds of
    /// commits, vs ~1 KB/row of actual data). Returns the wall time.
    pub async fn gc(&self) -> Result<std::time::Duration> {
        let started = std::time::Instant::now();
        sqlx::query("SELECT dolt_gc()")
            .execute(&self.pool)
            .await
            .context("dolt_gc")?;
        Ok(started.elapsed())
    }

    /// Upsert the (single) `scan_meta` row for the source.
    pub async fn write_scan_meta(&self, row: &ScanMetaRow, now: &str) -> Result<()> {
        let mut tx = self.pool.begin().await.context("begin scan_meta tx")?;
        bulk_upsert_in_tx(&mut tx, std::slice::from_ref(row), now).await?;
        tx.commit().await.context("commit scan_meta tx")?;
        Ok(())
    }

    /// Convenience wrapper around [`dr::record_object_error`] inside
    /// a fresh per-error tx. Used by the orchestrator to leave a
    /// durable trail for unreadable entries.
    pub async fn record_error(&self, table: &str, id: &str, err: &str) -> Result<()> {
        let mut tx = self.pool.begin().await.context("begin record_error tx")?;
        dr::record_object_error(&mut tx, table, id, err).await?;
        tx.commit().await.context("commit record_error tx")?;
        Ok(())
    }
}
