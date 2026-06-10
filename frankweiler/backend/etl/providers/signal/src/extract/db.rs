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
//! Every `payload` column stores the raw prost-encoded `Frame` bytes
//! verbatim as a BLOB. That keeps the diff between two snapshots
//! exactly equal to what changed in Signal's wire-format frames,
//! without forcing a schema-mapping step into Extract. The Translate
//! pass (deferred) will crack frames open into `event_type` rows.
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
use chrono::{SecondsFormat, Utc};
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

    /// Returns true if a row with this snapshot Blake3 already
    /// exists. Callers use this to short-circuit before the
    /// expensive decrypt + walk.
    pub async fn snapshot_already_ingested(&self, blake3_hex: &str) -> Result<bool> {
        let row = sqlx::query("SELECT 1 FROM ingested_backups WHERE blake3 = ? LIMIT 1")
            .bind(blake3_hex)
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
        blake3_hex: &str,
        snapshot_dir: &str,
        total_byte_size: u64,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339_opts(SecondsFormat::Secs, true);
        sqlx::query(
            "INSERT OR IGNORE INTO ingested_backups
                 (blake3, snapshot_dir, total_byte_size, ingested_at)
             VALUES (?, ?, ?, ?)",
        )
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

    pub async fn upsert_account(&self, payload: &[u8]) -> Result<()> {
        let mut tx = self.pool.begin().await.context("begin account tx")?;
        sqlx::query(
            "INSERT INTO account (id, payload) VALUES ('self', ?)
             ON CONFLICT(id) DO UPDATE SET payload = excluded.payload",
        )
        .bind(payload)
        .execute(&mut *tx)
        .await
        .context("upsert account")?;
        dr::record_object_attempt(&mut tx, "account", "self", None).await?;
        tx.commit().await.context("commit account tx")?;
        Ok(())
    }

    pub async fn upsert_recipient(
        &self,
        id: &str,
        identifier: Option<&str>,
        display_name: Option<&str>,
        payload: &[u8],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await.context("begin recipient tx")?;
        sqlx::query(
            "INSERT INTO recipients (id, identifier, display_name, payload)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                identifier = excluded.identifier,
                display_name = excluded.display_name,
                payload = excluded.payload",
        )
        .bind(id)
        .bind(identifier)
        .bind(display_name)
        .bind(payload)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("upsert recipient {id}"))?;
        dr::record_object_attempt(&mut tx, "recipients", id, None).await?;
        tx.commit().await.context("commit recipient tx")?;
        Ok(())
    }

    pub async fn upsert_chat(&self, id: &str, recipient_id: &str, payload: &[u8]) -> Result<()> {
        let mut tx = self.pool.begin().await.context("begin chat tx")?;
        sqlx::query(
            "INSERT INTO chats (id, recipient_id, payload) VALUES (?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                recipient_id = excluded.recipient_id,
                payload = excluded.payload",
        )
        .bind(id)
        .bind(recipient_id)
        .bind(payload)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("upsert chat {id}"))?;
        dr::record_object_attempt(&mut tx, "chats", id, None).await?;
        tx.commit().await.context("commit chat tx")?;
        Ok(())
    }

    pub async fn upsert_chat_item(
        &self,
        id: &str,
        chat_id: &str,
        author_id: &str,
        date_sent: i64,
        payload: &[u8],
    ) -> Result<()> {
        let mut tx = self.pool.begin().await.context("begin chat_item tx")?;
        sqlx::query(
            "INSERT INTO chat_items (id, chat_id, author_id, date_sent, payload)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                chat_id = excluded.chat_id,
                author_id = excluded.author_id,
                date_sent = excluded.date_sent,
                payload = excluded.payload",
        )
        .bind(id)
        .bind(chat_id)
        .bind(author_id)
        .bind(date_sent)
        .bind(payload)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("upsert chat_item {id}"))?;
        dr::record_object_attempt(&mut tx, "chat_items", id, None).await?;
        tx.commit().await.context("commit chat_item tx")?;
        Ok(())
    }
}
