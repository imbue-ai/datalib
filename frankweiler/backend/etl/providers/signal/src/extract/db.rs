//! Doltlite-backed raw store for the Signal provider.
//!
//! Four object tables, keyed by Signal's natural ids so re-fetches
//! across snapshots dedupe cleanly:
//!
//!   * `account`    — one row, `id = 'self'`. The account proto frame.
//!   * `recipients` — PK = the in-backup `recipient_id` (`uint64`).
//!     Promoted columns: `identifier` (e164 / aci hex), `display_name`.
//!   * `chats`      — PK = `chat_id`. `recipient_id` promoted for joins.
//!   * `chat_items` — PK = `"{chat_id}#{author_id}#{date_sent}"`.
//!     Promoted columns let SQL queries filter/sort without cracking
//!     the protobuf payload open.
//!
//! Every `payload` column stores the upstream `Frame::*` message
//! as JSONB. The transcoding from prost wire bytes to JSON is
//! lossless (see `tools/prost_toolchain/BUILD.bazel` for how the
//! prost types get serde derives), so `dolt diff` between two
//! snapshots still reflects only what changed in Signal's
//! wire-format frames. See `docs/data_architecture_ingestion.md`
//! §"Wire-fidelity of the raw store" for the principle.
//!
//! Row structs (`AccountRow`, `RecipientRow`, `ChatRow`,
//! `ChatItemRow`) live in `schema_raw.rs` next to their DDL
//! constants, and each impls `BulkUpsertable` so the generic
//! `frankweiler_etl::bulk::bulk_upsert_in_tx` helper writes them
//! all through the same chunked multi-row UPSERT.
//!
//! Attachment bytes (when an Extract enhancement starts harvesting
//! `Frame::Attachment` from the snapshot's `files/` tree) belong in
//! the sibling per-source CAS file managed by
//! [`frankweiler_etl::blob_cas`], with `blob_refs` rows in this entity
//! db pointing into it. The CAS handle is opened in [`RawDb::open`]
//! and exposed via [`RawDb::cas`] so that future code has the plumbing
//! ready — the same shape every other media-bearing provider
//! (slack, beeper, anthropic, chatgpt, notion, email) follows.

use std::path::Path;

use anyhow::{Context, Result};
use frankweiler_time::IsoOffsetTimestamp;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use frankweiler_etl::blob_cas::{self, BlobCas, RefStub};
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

    // ── blobs (delegate to shared `blob_cas`) ───────────────────────

    pub async fn blob_exists(&self, ref_id: &str) -> Result<bool> {
        blob_cas::ref_has_hash(&self.pool, ref_id).await
    }

    pub async fn store_blob(&self, stub: &RefStub<'_>, bytes: &[u8]) -> Result<String> {
        blob_cas::store_bytes(&self.pool, &self.cas, stub, bytes).await
    }

    pub async fn record_blob_error(
        &self,
        ref_id: &str,
        owning_id: &str,
        slot: &str,
        err: &str,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await.context("begin blob error tx")?;
        blob_cas::record_ref_error(&mut tx, ref_id, owning_id, slot, err).await?;
        tx.commit().await.context("commit blob error tx")?;
        Ok(())
    }
}
