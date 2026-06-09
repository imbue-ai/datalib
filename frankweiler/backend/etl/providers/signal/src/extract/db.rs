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
use sqlx::sqlite::SqlitePool;

use frankweiler_etl::blob_cas::{self, BlobCas, RefStub};
use frankweiler_etl::doltlite_raw::{self as dr};

pub use frankweiler_etl::doltlite_raw::db_path_for;

const DATA_TABLES: &[&str] = &["account", "recipients", "chats", "chat_items"];

const DDL_DATA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS account (
        id TEXT PRIMARY KEY,
        payload BLOB NULL
    )",
    "CREATE TABLE IF NOT EXISTS recipients (
        id TEXT PRIMARY KEY,
        identifier TEXT NULL,
        display_name TEXT NULL,
        payload BLOB NULL
    )",
    "CREATE TABLE IF NOT EXISTS chats (
        id TEXT PRIMARY KEY,
        recipient_id TEXT NOT NULL,
        payload BLOB NULL
    )",
    "CREATE INDEX IF NOT EXISTS chats_by_recipient ON chats(recipient_id)",
    "CREATE TABLE IF NOT EXISTS chat_items (
        id TEXT PRIMARY KEY,
        chat_id TEXT NOT NULL,
        author_id TEXT NOT NULL,
        date_sent INTEGER NOT NULL,
        payload BLOB NULL
    )",
    "CREATE INDEX IF NOT EXISTS chat_items_by_chat ON chat_items(chat_id, date_sent)",
];

fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = DDL_DATA.iter().map(|s| (*s).to_string()).collect();
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}

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
        Ok(())
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
