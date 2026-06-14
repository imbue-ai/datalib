//! Open + non-DDL data-manipulation for the Signal raw store.
//!
//! [`RawDb`] owns the entity-db pool and the sibling CAS handle.
//! The schema itself — every table DDL, every row struct + its
//! `BulkUpsertable` impl, the resume cursor, and the per-table
//! commentary — lives next door in [`super::schema_raw`].
//!
//! What's here is the small set of things `schema_raw` can't be:
//! `RawDb::open`, `reset`, the resume-cursor read/write methods
//! (`snapshot_already_ingested`, `record_snapshot_ingested`,
//! `last_ingested_snapshot`). Entity-table writes go through the
//! generic `frankweiler_etl::bulk::bulk_upsert_in_tx<T>` helper
//! from the caller (`super::mod`); they don't live on `RawDb`.

use std::path::Path;

use anyhow::{Context, Result};
use frankweiler_time::IsoOffsetTimestamp;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use frankweiler_etl::blob_cas::{self, BlobCas};
use frankweiler_etl::doltlite_raw::{self as dr};

use super::schema_raw::{full_ddl, DATA_TABLES};

pub use frankweiler_etl::doltlite_raw::db_path_for;

#[derive(Clone, Debug)]
pub struct RawDb {
    pool: SqlitePool,
    cas: BlobCas,
}

impl RawDb {
    pub async fn open(db_path: &Path) -> Result<Self> {
        let owned = full_ddl();
        let slices: Vec<&str> = owned.iter().map(String::as_str).collect();
        let pool = dr::open(db_path, &slices).await?;
        let cas = BlobCas::open(&blob_cas::cas_path_for(db_path)).await?;
        Ok(Self { pool, cas })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn cas(&self) -> &BlobCas {
        &self.cas
    }

    pub async fn reset(&self) -> Result<()> {
        dr::truncate_data_tables(&self.pool, DATA_TABLES).await?;
        // The resume cursor isn't a "data table" (no bookkeeping
        // sidecar, no upstream id) so it's not in DATA_TABLES; wipe
        // it explicitly so --reset-and-redownload re-ingests the
        // current snapshot.
        sqlx::query("DELETE FROM ingested_backups")
            .execute(&self.pool)
            .await
            .context("truncate ingested_backups")?;
        Ok(())
    }

    // ── ingested_backups (resume cursor) ────────────────────────────

    /// Returns true if a row with this snapshot fingerprint already
    /// exists. Cheap (single PK lookup against the fingerprint
    /// computed from `stat()` alone, no body I/O) — callers use this
    /// to short-circuit before any decrypt/blake3 work.
    pub async fn snapshot_already_ingested(&self, fingerprint: &str) -> Result<bool> {
        let row = sqlx::query("SELECT 1 FROM ingested_backups WHERE fingerprint = ? LIMIT 1")
            .bind(fingerprint)
            .fetch_optional(&self.pool)
            .await
            .context("snapshot_already_ingested")?;
        Ok(row.is_some())
    }

    /// Record a successful ingestion. Idempotent (`INSERT OR IGNORE`)
    /// so re-running after a partial-then-recovered ingest doesn't
    /// fail loudly.
    pub async fn record_snapshot_ingested(
        &self,
        fingerprint: &str,
        blake3_hex: &str,
        snapshot_dir: &str,
        total_byte_size: u64,
    ) -> Result<()> {
        let now = IsoOffsetTimestamp::now_local().to_rfc3339_secs();
        sqlx::query(
            "INSERT OR IGNORE INTO ingested_backups
                 (fingerprint, blake3, snapshot_dir, total_byte_size, ingested_at)
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(fingerprint)
        .bind(blake3_hex)
        .bind(snapshot_dir)
        .bind(total_byte_size as i64)
        .bind(now)
        .execute(&self.pool)
        .await
        .context("record_snapshot_ingested")?;
        Ok(())
    }

    /// Returns the most recently-recorded (snapshot_dir, blake3),
    /// for logging "we ingested X previously" lines. Returns `None`
    /// on a fresh DB.
    pub async fn last_ingested_snapshot(&self) -> Result<Option<(String, String)>> {
        let row = sqlx::query(
            "SELECT snapshot_dir, blake3 FROM ingested_backups
             ORDER BY ingested_at DESC LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await
        .context("last_ingested_snapshot")?;
        Ok(row.and_then(|r| {
            let d: Option<String> = r.try_get("snapshot_dir").ok();
            let b: String = r.try_get("blake3").ok()?;
            Some((d.unwrap_or_default(), b))
        }))
    }
}
