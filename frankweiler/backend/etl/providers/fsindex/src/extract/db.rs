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
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use frankweiler_etl::bulk::bulk_upsert_in_tx;
use frankweiler_etl::doltlite_raw as dr;

use super::schema_raw::{full_ddl, FileRow, FileStatsRow, ScanMetaRow, StampKind, DATA_TABLES};

/// Default write chunk size for `bulk_write_scan`. Keeps any one
/// transaction small enough that an interrupt loses bounded work.
const SCAN_WRITE_CHUNK: usize = 5_000;

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

    /// Load the prior scan's `file_stats` rows keyed by id, for the
    /// fast-rescan compare. FIXME(load-all-stats): at the
    /// tens-of-millions design target this is ~80 B/entry × 50M ≈
    /// 4 GB. Acceptable on dev hardware; a future streaming-cursor
    /// path may be needed.
    pub async fn load_prev_stats(&self) -> Result<HashMap<String, FileStatsRow>> {
        let rows = sqlx::query(
            "SELECT id, mtime_ns, size, stamp_kind, inode, dev, ctime_ns FROM file_stats",
        )
        .fetch_all(&self.pool)
        .await
        .context("select file_stats")?;
        let mut out = HashMap::with_capacity(rows.len());
        for r in rows {
            let id: String = r.try_get("id").context("read file_stats.id")?;
            let stamp_str: String = r
                .try_get("stamp_kind")
                .unwrap_or_else(|_| "rescan".to_string());
            let stamp_kind = match stamp_str.as_str() {
                "inode" => StampKind::Inode,
                "nostamp" => StampKind::NoStamp,
                _ => StampKind::Rescan,
            };
            let row = FileStatsRow {
                id: id.clone(),
                mtime_ns: r.try_get("mtime_ns").unwrap_or(0),
                size: r.try_get("size").unwrap_or(0),
                stamp_kind,
                inode: r.try_get("inode").ok(),
                dev: r.try_get("dev").ok(),
                ctime_ns: r.try_get("ctime_ns").ok(),
            };
            out.insert(id, row);
        }
        Ok(out)
    }

    /// Write the whole scan in chunked transactions. Each chunk
    /// commits independently so a mid-write interrupt loses bounded
    /// work. `bulk_upsert_in_tx` auto-stamps `<table>_bookkeeping`.
    pub async fn bulk_write_scan(
        &self,
        files: &[FileRow],
        stats: &[FileStatsRow],
        scan_meta: &ScanMetaRow,
        now: &str,
    ) -> Result<()> {
        for chunk in files.chunks(SCAN_WRITE_CHUNK) {
            let mut tx = self.pool.begin().await.context("begin files tx")?;
            bulk_upsert_in_tx(&mut tx, chunk, now).await?;
            tx.commit().await.context("commit files tx")?;
        }
        for chunk in stats.chunks(SCAN_WRITE_CHUNK) {
            let mut tx = self.pool.begin().await.context("begin file_stats tx")?;
            bulk_upsert_in_tx(&mut tx, chunk, now).await?;
            tx.commit().await.context("commit file_stats tx")?;
        }
        let mut tx = self.pool.begin().await.context("begin scan_meta tx")?;
        bulk_upsert_in_tx(&mut tx, std::slice::from_ref(scan_meta), now).await?;
        tx.commit().await.context("commit scan_meta tx")?;
        Ok(())
    }

    /// Load `(id → blake3)` for every prior `files` row whose `kind`
    /// is `'file'`. This is the cache the fast-rescan trick reads
    /// against: when `stamp::decide` says ReuseHash, the walker
    /// looks up the cached blake3 here instead of reopening the
    /// file. Dirs are excluded because their hash always recomputes
    /// from children; symlinks are excluded because re-hashing the
    /// target string is free.
    pub async fn load_prev_file_blake3s(&self) -> Result<HashMap<String, String>> {
        let rows = sqlx::query("SELECT id, blake3 FROM files WHERE kind = 'file'")
            .fetch_all(&self.pool)
            .await
            .context("select files (file kind)")?;
        let mut out = HashMap::with_capacity(rows.len());
        for r in rows {
            let id: String = r.try_get("id").context("read files.id")?;
            let blake3: String = r.try_get("blake3").context("read files.blake3")?;
            out.insert(id, blake3);
        }
        Ok(out)
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
