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

    // ── conversations: listing skip-check ──────────────────────────

    /// Bulk-read `(id → update_time)` for the listed ids. Returns one
    /// entry per *existing* row (with a non-null `update_time`). Missing
    /// ids are absent from the map — caller treats them as "we don't
    /// have this conversation yet, fetch it." Used by the listing pass
    /// to decide which conversations need a detail fetch.
    ///
    /// `update_time` is JSON-encoded — same encoding the upstream
    /// listing returns (a number for chatgpt, but we round-trip
    /// through `serde_json::to_string` for comparison-stability
    /// against `string` / `null` variants the API has been known to
    /// emit).
    pub async fn existing_update_times(&self, ids: &[&str]) -> Result<HashMap<String, String>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT id, update_time FROM conversations \
              WHERE id IN ({placeholders}) AND update_time IS NOT NULL"
        );
        let mut q = sqlx::query(&sql);
        for id in ids {
            q = q.bind(*id);
        }
        let rows = q
            .fetch_all(&self.pool)
            .await
            .context("existing_update_times")?;
        let mut out = HashMap::with_capacity(rows.len());
        for r in &rows {
            let id: String = r.try_get("id").unwrap_or_default();
            if let Ok(ut) = r.try_get::<String, _>("update_time") {
                out.insert(id, ut);
            }
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

    /// Snapshot `(file_id → blake3)` for every attachment whose bytes
    /// have ever landed in the CAS. Loaded once at the start of a
    /// fetch run; updated in-place as new downloads land. Replaces
    /// the per-file SQL `attachment_has_bytes` lookup.
    pub async fn load_attachment_blake3s(&self) -> Result<HashMap<String, String>> {
        frankweiler_etl::blob_cas::load_blake3_index(&self.pool, "chatgpt_attachments", "file_id")
            .await
    }

    // ── loads ───────────────────────────────────────────────────────

    /// Conversation payloads + their fetch-time metadata. The payload
    /// is the raw upstream response; downstream layers stamp synthetic
    /// keys back on if they want them.
    pub async fn load_conversations(&self) -> Result<Vec<LoadedConversation>> {
        let rows = sqlx::query(
            "SELECT c.id, json(c.payload) AS payload, b.fetched_at
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
            let Ok(payload_v) = serde_json::from_str::<Value>(&payload) else {
                continue;
            };
            out.push(LoadedConversation {
                id,
                payload: payload_v,
                fetched_at,
            });
        }
        Ok(out)
    }
}

/// One row's worth of loaded conversation data — payload plus the
/// fetch timestamp. Rows only exist post-detail-fetch.
#[derive(Debug, Clone)]
pub struct LoadedConversation {
    pub id: String,
    pub payload: Value,
    pub fetched_at: Option<String>,
}

/// Bag returned to the synchronous translate / synthesize path.
/// Attachment bytes are no longer carried alongside — translate's
/// `parse` loads a per-doc [`BlobBundle`] for each conversation,
/// keeping render fully sync.
#[derive(Clone, Default)]
pub struct LoadedRaw {
    pub me: Option<Value>,
    pub conversations: Vec<LoadedConversation>,
}

/// Synchronous helper for tests that want a snapshot of every entity
/// table at a fixed point in time. Production translate uses
/// `crate::translate::parse::parse(..., last_render_hash)` instead;
/// this one ignores the cursor and loads everything. Attachment bytes
/// are NOT loaded here — tests that need them load a [`BlobBundle`]
/// via `BlobBundle::load(...)` directly.
pub fn block_on_load_all(db_path: &Path) -> Result<LoadedRaw> {
    let path = db_path.to_path_buf();
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let db = RawDb::open(&path).await?;
            Ok::<_, anyhow::Error>(LoadedRaw {
                me: db.load_me().await?,
                conversations: db.load_conversations().await?,
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
    async fn existing_update_times_round_trips() {
        use crate::extract::schema_raw::ConversationRow;
        use frankweiler_etl::bulk::bulk_upsert_in_tx;
        use frankweiler_etl::doltlite_raw::WirePayload;
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("c.doltlite_db")).await.unwrap();
        let mut tx = db.pool().begin().await.unwrap();
        bulk_upsert_in_tx(
            &mut tx,
            &[ConversationRow {
                id_and_payload: WirePayload {
                    id: "c1".into(),
                    payload: serde_json::to_string(&json!({"id":"c1","mapping":{}})).unwrap(),
                },
                title: Some("T".into()),
                // upstream listing emits update_time as a number; we
                // JSON-encode for comparison-stability.
                update_time: Some("1.0".into()),
            }],
            "2026-06-11T00:00:00-07:00",
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();

        let map = db.existing_update_times(&["c1", "missing"]).await.unwrap();
        assert_eq!(map.get("c1").map(String::as_str), Some("1.0"));
        assert!(!map.contains_key("missing"));
    }
}
