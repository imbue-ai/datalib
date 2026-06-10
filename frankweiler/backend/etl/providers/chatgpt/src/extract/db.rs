//! Doltlite-backed raw store for the ChatGPT provider.
//!
//! Replaces the JSON tree of `me.json` + `conversations.json` +
//! `conversations/<id>.json` with a single sqlite file at
//! `<data_root>/raw/<name>.doltlite_db`. Shared bookkeeping tables
//! (`blobs`, `sync_runs`) and open/blob plumbing
//! live in [`frankweiler_etl::doltlite_raw`]; the primary-key policy
//! that governs every object table here is documented there.
//!
//! Tables:
//! - `me` — PK is the upstream account id from `/backend-api/me`.
//! - `conversations` — PK is the upstream conversation id.
//!   `last_listing_update_time` is the column we used to stuff into the
//!   JSON body as a synthetic `_listing_update_time` key; promoting it
//!   to its own column keeps the payload byte-for-byte identical to the
//!   live API.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use frankweiler_etl::blob_cas::{
    self, BlobCas, BlobReader, InMemoryBlobReader, RefStub, SqliteBlobReader,
};
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

/// One row of input to [`RawDb::upsert_conversation_detail`]. `payload`
/// is the raw `/backend-api/conversation/{id}` response, **without** any
/// downloader-synthesized keys.
#[derive(Debug, Clone)]
pub struct ConversationDetail {
    pub id: String,
    pub title: Option<String>,
    pub update_time: Option<String>,
    pub last_listing_update_time: Option<Value>,
    pub payload: String,
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

    /// Wipe every per-row table so the next fetch re-downloads
    /// everything from upstream. See
    /// [`frankweiler_etl::doltlite_raw::truncate_data_tables`].
    pub async fn reset(&self) -> Result<()> {
        dr::truncate_data_tables(&self.pool, DATA_TABLES).await
    }

    // ── `me` ────────────────────────────────────────────────────────

    /// Upsert the `/me` row by upstream account id. We pluck `email` /
    /// `name` for cheap predicate queries; `payload` carries the full
    /// response unchanged.
    pub async fn upsert_me(&self, payload: &Value) -> Result<()> {
        let id = payload
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("/me response missing id"))?;
        let email = payload.get("email").and_then(|v| v.as_str());
        let name = payload.get("name").and_then(|v| v.as_str());
        let payload_str = serde_json::to_string(payload).context("serialize /me")?;
        let mut tx = self.pool.begin().await.context("begin upsert_me tx")?;
        sqlx::query(
            "INSERT INTO me (id, email, name, payload)
             VALUES (?, ?, ?, jsonb(?))
             ON CONFLICT(id) DO UPDATE SET
                email = COALESCE(excluded.email, me.email),
                name = COALESCE(excluded.name, me.name),
                payload = excluded.payload",
        )
        .bind(id)
        .bind(email)
        .bind(name)
        .bind(&payload_str)
        .execute(&mut *tx)
        .await
        .context("upsert me")?;
        dr::record_object_attempt(&mut tx, "me", id, None).await?;
        tx.commit().await.context("commit upsert_me tx")?;
        Ok(())
    }

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

    // ── conversations: pre-seed + skip-check ───────────────────────

    /// Pre-seed `(id, listing-derived metadata)` for every entry in a
    /// listing page. Existing rows keep their `payload` intact; we just
    /// refresh `last_listing_update_time` so the skip-check on the next
    /// run sees the freshest value.
    pub async fn pre_seed_conversations(&self, items: &[&Value]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.context("begin pre_seed tx")?;
        for item in items {
            let Some(id) = item.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            let title = item.get("title").and_then(|v| v.as_str());
            // update_time on a listing item is sometimes a number, so
            // we stringify defensively.
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
            // Always-paired sidecar: ensure a bookkeeping row exists.
            sqlx::query("INSERT OR IGNORE INTO conversations_bookkeeping (id) VALUES (?)")
                .bind(id)
                .execute(&mut *tx)
                .await
                .with_context(|| format!("pre_seed conversation bookkeeping {id}"))?;
        }
        tx.commit().await.context("commit pre_seed tx")?;
        Ok(())
    }

    /// Snapshot every conversation's listing-update-time + payload
    /// presence so the extract loop can decide which detail fetches to
    /// skip. Stored as JSON text — decoded back into a [`Value`] so
    /// equality compares "as the API would" rather than as raw strings.
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

    // ── conversations: detail upsert ───────────────────────────────

    /// Upsert a full `/backend-api/conversation/{id}` response. Clears
    /// `last_error` on success.
    pub async fn upsert_conversation_detail(&self, row: &ConversationDetail) -> Result<()> {
        let listing_ut_str = row
            .last_listing_update_time
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default());
        let mut tx = self
            .pool
            .begin()
            .await
            .context("begin upsert_conversation_detail tx")?;
        sqlx::query(
            "INSERT INTO conversations (id, title, update_time, last_listing_update_time, payload)
             VALUES (?, ?, ?, ?, jsonb(?))
             ON CONFLICT(id) DO UPDATE SET
                title = COALESCE(excluded.title, conversations.title),
                update_time = COALESCE(excluded.update_time, conversations.update_time),
                last_listing_update_time = COALESCE(excluded.last_listing_update_time, conversations.last_listing_update_time),
                payload = excluded.payload",
        )
        .bind(&row.id)
        .bind(row.title.as_deref())
        .bind(row.update_time.as_deref())
        .bind(listing_ut_str.as_deref())
        .bind(&row.payload)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("upsert conversation {}", row.id))?;
        dr::record_object_attempt(&mut tx, "conversations", &row.id, None).await?;
        tx.commit()
            .await
            .context("commit upsert_conversation_detail tx")?;
        Ok(())
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

    // ── loads ───────────────────────────────────────────────────────

    /// Conversation payloads + their fetch-time metadata. The payload
    /// is the raw upstream response; downstream layers stamp synthetic
    /// keys back on if they want them.
    pub async fn load_conversations(&self) -> Result<Vec<LoadedConversation>> {
        // `fetched_at` lives on the bookkeeping sidecar; LEFT JOIN so a
        // pre-seeded row (no payload yet) still wouldn't surface here
        // (filtered by payload IS NOT NULL).
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

    // ── blobs (delegate to shared `blob_cas`) ───────────────────────

    pub async fn blob_exists(&self, ref_id: &str) -> Result<bool> {
        blob_cas::ref_has_hash(&self.pool, ref_id).await
    }

    /// Stash fetched bytes into the CAS and attach the resulting hash
    /// to the named ref. One call per successful download.
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
    pub blobs: Arc<dyn BlobReader>,
}

impl Default for LoadedRaw {
    fn default() -> Self {
        Self {
            me: None,
            conversations: Vec::new(),
            blobs: InMemoryBlobReader::empty_handle(),
        }
    }
}

/// Synchronous helper for non-async callers (translate, synthesize)
/// that already run under `#[tokio::main]`. Uses `block_in_place` +
/// the current Handle, so it must be invoked on a multi-thread runtime.
pub fn block_on_load_all(db_path: &Path) -> Result<LoadedRaw> {
    let path = db_path.to_path_buf();
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let db = RawDb::open(&path).await?;
            let blobs: Arc<dyn BlobReader> = Arc::new(SqliteBlobReader::new(
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
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("c.doltlite_db")).await.unwrap();
        db.upsert_me(&json!({"id": "u1", "email": "x@y", "name": "X Y"}))
            .await
            .unwrap();
        let me = db.load_me().await.unwrap().expect("me present");
        assert_eq!(me["id"], "u1");
        assert_eq!(me["email"], "x@y");
    }

    #[tokio::test]
    async fn conversation_detail_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("c.doltlite_db")).await.unwrap();
        db.upsert_conversation_detail(&ConversationDetail {
            id: "c1".into(),
            title: Some("Hi".into()),
            update_time: Some("2026-01-01T00:00:00+00:00".into()),
            last_listing_update_time: Some(json!(123.456)),
            payload: serde_json::to_string(&json!({"id":"c1","mapping":{}})).unwrap(),
        })
        .await
        .unwrap();
        let convs = db.load_conversations().await.unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].id, "c1");
        assert_eq!(convs[0].last_listing_update_time, Some(json!(123.456)));
    }

    #[tokio::test]
    async fn pre_seed_and_skip_check() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("c.doltlite_db")).await.unwrap();
        let listing = [json!({"id":"c1","title":"T","update_time":1.0})];
        let refs: Vec<&Value> = listing.iter().collect();
        db.pre_seed_conversations(&refs).await.unwrap();
        let states = db.conversation_states().await.unwrap();
        assert!(states.contains_key("c1"));
        assert!(!states["c1"].has_payload);
        assert_eq!(states["c1"].last_listing_update_time, Some(json!(1.0)));
    }
}
