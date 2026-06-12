//! Doltlite-backed raw store for the ChatGPT provider.
//!
//! Three tables — `me`, `conversations`, `chatgpt_attachments` —
//! shared bookkeeping (`<table>_bookkeeping`, `sync_runs`, …) lives
//! in [`frankweiler_etl::doltlite_raw`]. Per the dolt_diff +
//! per-provider CAS edge migration: attachment bytes still ride in
//! the shared `cas_objects`, but the (file_id → blake3) mapping lives
//! on `chatgpt_attachments` rather than the shared `blob_refs`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use frankweiler_etl::blob_cas::BlobCas;
use frankweiler_etl::doltlite_raw::{self as dr};

use super::schema_raw::{full_ddl, DATA_TABLES};

pub use frankweiler_etl::doltlite_raw::db_path_for;

/// One row's worth of "what does the listing pass know about this
/// conversation right now". Used to decide whether to short-circuit a
/// detail fetch.
#[derive(Debug, Clone, Default)]
pub struct ConvState {
    pub last_listing_update_time: Option<Value>,
    pub has_payload: bool,
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
        let cas = BlobCas::open(&frankweiler_etl::blob_cas::cas_path_for(db_path)).await?;
        Ok(Self { pool, cas })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn cas(&self) -> &BlobCas {
        &self.cas
    }

    /// Wipe every per-row table so the next fetch re-downloads
    /// everything from upstream.
    pub async fn reset(&self) -> Result<()> {
        dr::truncate_data_tables(&self.pool, DATA_TABLES).await
    }

    /// Replaces `truncate_blob_refs` for this provider: clear the
    /// per-provider `blake3` column so the next walk re-decodes and
    /// re-stores. The CAS bytes themselves stay — `put_many` is
    /// INSERT OR IGNORE, re-hashing the same bytes lands on the same
    /// blake3.
    pub async fn clear_blob_hashes(&self) -> Result<()> {
        sqlx::query("UPDATE chatgpt_attachments SET blake3 = NULL")
            .execute(&self.pool)
            .await
            .context("clear chatgpt_attachments.blake3")?;
        Ok(())
    }

    // ── `me` ────────────────────────────────────────────────────────

    /// Returns the latest `/me` payload, if any.
    pub async fn load_me(&self) -> Result<Option<Value>> {
        let row = sqlx::query("SELECT json(payload) AS payload FROM me ORDER BY id LIMIT 1")
            .fetch_optional(&self.pool)
            .await
            .context("select me")?;
        let Some(row) = row else { return Ok(None) };
        let payload: Option<String> = row.try_get("payload").ok();
        Ok(payload.and_then(|s: String| serde_json::from_str(&s).ok()))
    }

    // ── conversations: listing pre-seed + skip-check ───────────────

    /// Pre-seed `(id, listing-derived metadata)` for every entry in a
    /// listing page. Existing rows keep their `payload` intact; we
    /// just refresh `last_listing_update_time`. Lightweight SQL —
    /// keeps the surface predictable since these rows may not have a
    /// payload yet (so the bulk-upsert path with PAYLOAD_COLUMN =
    /// Some can't represent them).
    pub async fn pre_seed_conversations(&self, items: &[&Value], now: &str) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.context("begin pre_seed tx")?;
        for item in items {
            let Some(id) = item.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            let title = item.get("title").and_then(|v| v.as_str());
            let listing_ut = item.get("update_time").cloned();
            let listing_ut_str = listing_ut
                .as_ref()
                .map(|v| serde_json::to_string(v).unwrap_or_default());
            sqlx::query(
                "INSERT INTO conversations (id, title, last_listing_update_time)
                 VALUES (?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET
                    title = COALESCE(excluded.title, conversations.title),
                    last_listing_update_time = COALESCE(excluded.last_listing_update_time, conversations.last_listing_update_time)",
            )
            .bind(id)
            .bind(title)
            .bind(listing_ut_str.as_deref())
            .execute(&mut *tx)
            .await
            .with_context(|| format!("pre_seed conversation {id}"))?;
            dr::record_object_attempt(&mut tx, "conversations", id, Some(now)).await?;
        }
        tx.commit().await.context("commit pre_seed tx")?;
        Ok(())
    }

    pub async fn conversation_states(&self) -> Result<HashMap<String, ConvState>> {
        let rows = sqlx::query(
            "SELECT id, last_listing_update_time, payload IS NOT NULL AS has_payload
             FROM conversations",
        )
        .fetch_all(&self.pool)
        .await
        .context("select conversation_states")?;
        let mut out = HashMap::with_capacity(rows.len());
        for r in rows {
            let id: String = r.try_get("id").unwrap_or_default();
            let llut: Option<String> = r.try_get("last_listing_update_time").ok();
            let has: i64 = r.try_get("has_payload").unwrap_or(0);
            let llut_value = llut.and_then(|s: String| serde_json::from_str::<Value>(&s).ok());
            out.insert(
                id,
                ConvState {
                    last_listing_update_time: llut_value,
                    has_payload: has != 0,
                },
            );
        }
        Ok(out)
    }

    pub async fn record_conversation_error(&self, id: &str, err: &str) -> Result<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .context("begin record_conversation_error tx")?;
        dr::record_object_error(&mut tx, "conversations", id, err).await?;
        tx.commit()
            .await
            .context("commit record_conversation_error tx")?;
        Ok(())
    }

    pub async fn failed_conversation_ids(&self) -> Result<Vec<String>> {
        dr::failed_ids(&self.pool, "conversations").await
    }

    /// `(file_id, blake3 IS NOT NULL)` lookup — true if we already
    /// have the bytes for this attachment somewhere in the CAS. Uses
    /// the per-file index so the per-attachment skip check is cheap.
    pub async fn attachment_has_bytes(&self, file_id: &str) -> Result<bool> {
        let row = sqlx::query(
            "SELECT 1 FROM chatgpt_attachments \
              WHERE file_id = ? AND blake3 IS NOT NULL LIMIT 1",
        )
        .bind(file_id)
        .fetch_optional(&self.pool)
        .await
        .with_context(|| format!("attachment_has_bytes {file_id}"))?;
        Ok(row.is_some())
    }

    // ── loads ───────────────────────────────────────────────────────

    /// Conversation payloads + their fetch-time metadata. The payload
    /// is the raw upstream response; downstream layers stamp synthetic
    /// keys back on if they want them.
    pub async fn load_conversations(&self) -> Result<Vec<LoadedConversation>> {
        let rows = sqlx::query(
            "SELECT c.id, json(c.payload) AS payload, b.fetched_at, c.last_listing_update_time
             FROM conversations c
             LEFT JOIN conversations_bookkeeping b ON b.id = c.id
             WHERE c.payload IS NOT NULL
             ORDER BY c.id",
        )
        .fetch_all(&self.pool)
        .await
        .context("select conversations")?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let id: String = r.try_get("id").unwrap_or_default();
            let payload: String = match r.try_get("payload") {
                Ok(s) => s,
                Err(_) => continue,
            };
            let fetched_at: Option<String> = r.try_get("fetched_at").ok();
            let llut_str: Option<String> = r.try_get("last_listing_update_time").ok();
            let llut = llut_str.and_then(|s: String| serde_json::from_str::<Value>(&s).ok());
            let Ok(payload_v) = serde_json::from_str::<Value>(&payload) else {
                continue;
            };
            out.push(LoadedConversation {
                id,
                payload: payload_v,
                fetched_at,
                last_listing_update_time: llut,
            });
        }
        Ok(out)
    }
}

/// One row's worth of loaded conversation data — payload plus the
/// metadata downstream consumers used to recover from the legacy
/// `_fetched_at` / `_listing_update_time` synthetic keys.
#[derive(Debug, Clone)]
pub struct LoadedConversation {
    pub id: String,
    pub payload: Value,
    pub fetched_at: Option<String>,
    pub last_listing_update_time: Option<Value>,
}

/// Bag returned to the synchronous translate / synthesize path. `blobs`
/// is a streaming handle so render can fetch one attachment at a time
/// rather than bulk-loading the whole table into memory.
#[derive(Clone)]
pub struct LoadedRaw {
    pub me: Option<Value>,
    pub conversations: Vec<LoadedConversation>,
    pub blobs: std::sync::Arc<dyn frankweiler_etl::blob_cas::BlobReader>,
}

impl Default for LoadedRaw {
    fn default() -> Self {
        Self {
            me: None,
            conversations: Vec::new(),
            blobs: frankweiler_etl::blob_cas::InMemoryBlobReader::empty_handle(),
        }
    }
}

/// Synchronous helper for tests that want a snapshot of every entity
/// table at a fixed point in time. Production translate uses
/// `crate::translate::parse::parse(..., last_render_hash)` instead;
/// this one ignores the cursor and loads everything.
pub fn block_on_load_all(db_path: &Path) -> Result<LoadedRaw> {
    let path = db_path.to_path_buf();
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let db = RawDb::open(&path).await?;
            let blobs: std::sync::Arc<dyn frankweiler_etl::blob_cas::BlobReader> =
                std::sync::Arc::new(crate::translate::blob_reader::ChatgptBlobReader::new(
                    db.pool().clone(),
                    db.cas().pool().clone(),
                ));
            Ok::<_, anyhow::Error>(LoadedRaw {
                me: db.load_me().await?,
                conversations: db.load_conversations().await?,
                blobs,
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn me_round_trips() {
        use crate::extract::schema_raw::MeRow;
        use frankweiler_etl::bulk::bulk_upsert_in_tx;
        use frankweiler_etl::doltlite_raw::WirePayload;
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("c.doltlite_db")).await.unwrap();
        let me = json!({"id": "u1", "email": "x@y", "name": "X Y"});
        let mut tx = db.pool().begin().await.unwrap();
        bulk_upsert_in_tx(
            &mut tx,
            &[MeRow {
                id_and_payload: WirePayload {
                    id: "u1".into(),
                    payload: serde_json::to_string(&me).unwrap(),
                },
                email: Some("x@y".into()),
                name: Some("X Y".into()),
            }],
            "2026-06-11T00:00:00-07:00",
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let loaded = db.load_me().await.unwrap().expect("me present");
        assert_eq!(loaded["id"], "u1");
        assert_eq!(loaded["email"], "x@y");
    }

    #[tokio::test]
    async fn pre_seed_and_skip_check() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("c.doltlite_db")).await.unwrap();
        let listing = [json!({"id":"c1","title":"T","update_time":1.0})];
        let refs: Vec<&Value> = listing.iter().collect();
        db.pre_seed_conversations(&refs, "2026-06-11T00:00:00-07:00")
            .await
            .unwrap();
        let states = db.conversation_states().await.unwrap();
        assert!(states.contains_key("c1"));
        assert!(!states["c1"].has_payload);
        assert_eq!(states["c1"].last_listing_update_time, Some(json!(1.0)));
    }
}
