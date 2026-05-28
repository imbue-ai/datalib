//! Doltlite-backed raw store for the Anthropic (Claude) provider.
//!
//! Replaces the JSON tree of `users.json` + `conversations.json`
//! (with the conversations pre-normalized into export shape) with a
//! single sqlite file at `<data_root>/raw/<name>.doltlite_db`.
//!
//! Per the design discussion: we now store conversations as the **raw**
//! `/api/...` response, *not* post-normalize. The translate step
//! applies `normalize_to_export_shape` at read time. This keeps the
//! raw store as close to the wire as possible — dolt diffs reflect
//! actual upstream change rather than churn introduced by our
//! normalizer's evolution.
//!
//! Shared bookkeeping tables (`blobs`, `endpoint_shapes`, `sync_runs`)
//! and open/blob plumbing live in [`frankweiler_etl::doltlite_raw`].

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::Value;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use frankweiler_etl::doltlite_raw::{self as dr};

pub use frankweiler_etl::doltlite_raw::{db_path_for, BlobBytes};

const DDL: &[&str] = &[
    // users — PK is the Anthropic user UUID. Carries the `users.json`
    // entries from the bulk export plus anything synthesized from
    // `/api/account` when no export is available.
    "CREATE TABLE IF NOT EXISTS users (
        id TEXT PRIMARY KEY,
        email TEXT NULL,
        full_name TEXT NULL,
        payload TEXT NULL,
        fetched_at TEXT NULL,
        attempt_count INTEGER NOT NULL DEFAULT 0,
        last_attempt_at TEXT NULL,
        last_error TEXT NULL
    )",
    // orgs — PK is the Anthropic organization UUID.
    "CREATE TABLE IF NOT EXISTS orgs (
        id TEXT PRIMARY KEY,
        name TEXT NULL,
        payload TEXT NULL,
        fetched_at TEXT NULL,
        attempt_count INTEGER NOT NULL DEFAULT 0,
        last_attempt_at TEXT NULL,
        last_error TEXT NULL
    )",
    // conversations — PK is the Anthropic conversation UUID. We store
    // the raw `/api/.../chat_conversations/{uuid}` payload here, NOT
    // the normalized export shape. `org_uuid` is the owning
    // organization (needed at read time to re-build the export-shape
    // `_source` block).
    "CREATE TABLE IF NOT EXISTS conversations (
        id TEXT PRIMARY KEY,
        org_uuid TEXT NULL,
        name TEXT NULL,
        updated_at TEXT NULL,
        payload TEXT NULL,
        fetched_at TEXT NULL,
        attempt_count INTEGER NOT NULL DEFAULT 0,
        last_attempt_at TEXT NULL,
        last_error TEXT NULL
    )",
    "CREATE INDEX IF NOT EXISTS conversations_org ON conversations(org_uuid)",
    "CREATE INDEX IF NOT EXISTS conversations_updated ON conversations(updated_at)",
];

#[derive(Clone)]
pub struct RawDb {
    pool: SqlitePool,
}

#[derive(Debug, Clone, Default)]
pub struct ConvState {
    pub updated_at: Option<String>,
    pub has_payload: bool,
}

#[derive(Debug, Clone)]
pub struct ConversationDetail {
    pub id: String,
    pub org_uuid: String,
    pub name: Option<String>,
    pub updated_at: Option<String>,
    /// Raw upstream `/api/.../chat_conversations/{uuid}` payload.
    pub payload: String,
}

impl RawDb {
    pub async fn open(db_path: &Path) -> Result<Self> {
        let pool = dr::open(db_path, DDL).await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn start_run(&self, config: &Value) -> Result<i64> {
        dr::start_run(&self.pool, config).await
    }

    pub async fn finish_run(&self, run_id: i64, status: &str, summary: &Value) -> Result<()> {
        dr::finish_run(&self.pool, run_id, status, summary).await
    }

    // ── users ──────────────────────────────────────────────────────

    /// Upsert one user row from a raw user object (matches the entries
    /// in `users.json` from a bulk export, or what `/api/account`
    /// returns).
    pub async fn upsert_user(&self, payload: &Value) -> Result<()> {
        let id = payload
            .get("uuid")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("user entry missing uuid"))?;
        let email = payload.get("email_address").and_then(|v| v.as_str());
        let full_name = payload.get("full_name").and_then(|v| v.as_str());
        let payload_str = serde_json::to_string(payload).context("serialize user")?;
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO users (id, email, full_name, payload, fetched_at, last_attempt_at, last_error)
             VALUES (?, ?, ?, ?, ?, ?, NULL)
             ON CONFLICT(id) DO UPDATE SET
                email = COALESCE(excluded.email, users.email),
                full_name = COALESCE(excluded.full_name, users.full_name),
                payload = excluded.payload,
                fetched_at = excluded.fetched_at,
                last_attempt_at = excluded.last_attempt_at,
                last_error = NULL",
        )
        .bind(id)
        .bind(email)
        .bind(full_name)
        .bind(&payload_str)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await
        .context("upsert user")?;
        Ok(())
    }

    pub async fn has_any_user(&self) -> Result<bool> {
        let row = sqlx::query("SELECT 1 FROM users LIMIT 1")
            .fetch_optional(&self.pool)
            .await
            .context("has_any_user")?;
        Ok(row.is_some())
    }

    pub async fn load_users(&self) -> Result<Vec<Value>> {
        dr::load_payloads(&self.pool, "users").await
    }

    /// First user's uuid, used to fill the `account.uuid` field on
    /// normalized conversations. Returns `None` if no user rows exist.
    pub async fn first_user_uuid(&self) -> Result<Option<String>> {
        let row = sqlx::query("SELECT id FROM users ORDER BY id LIMIT 1")
            .fetch_optional(&self.pool)
            .await
            .context("first_user_uuid")?;
        Ok(row.and_then(|r| r.try_get::<String, _>("id").ok()))
    }

    // ── orgs ───────────────────────────────────────────────────────

    pub async fn upsert_org(&self, payload: &Value) -> Result<()> {
        let id = payload
            .get("uuid")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("org missing uuid"))?;
        let name = payload.get("name").and_then(|v| v.as_str());
        let payload_str = serde_json::to_string(payload).context("serialize org")?;
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO orgs (id, name, payload, fetched_at, last_attempt_at, last_error)
             VALUES (?, ?, ?, ?, ?, NULL)
             ON CONFLICT(id) DO UPDATE SET
                name = COALESCE(excluded.name, orgs.name),
                payload = excluded.payload,
                fetched_at = excluded.fetched_at,
                last_attempt_at = excluded.last_attempt_at,
                last_error = NULL",
        )
        .bind(id)
        .bind(name)
        .bind(&payload_str)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await
        .context("upsert org")?;
        Ok(())
    }

    // ── conversations ──────────────────────────────────────────────

    /// Pre-seed listing-derived rows. Existing rows keep their
    /// `payload` intact; only `updated_at` is refreshed so the
    /// skip-check on the next run has the freshest signal.
    pub async fn pre_seed_conversations(&self, items: &[(&str, &Value)]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.context("begin pre_seed tx")?;
        for (org_uuid, item) in items {
            let Some(id) = item.get("uuid").and_then(|v| v.as_str()) else {
                continue;
            };
            let name = item.get("name").and_then(|v| v.as_str());
            let updated = item.get("updated_at").and_then(|v| v.as_str());
            sqlx::query(
                "INSERT INTO conversations (id, org_uuid, name, updated_at)
                 VALUES (?, ?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET
                    org_uuid = COALESCE(excluded.org_uuid, conversations.org_uuid),
                    name = COALESCE(excluded.name, conversations.name),
                    updated_at = COALESCE(excluded.updated_at, conversations.updated_at)",
            )
            .bind(id)
            .bind(org_uuid)
            .bind(name)
            .bind(updated)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("pre_seed conv {id}"))?;
        }
        tx.commit().await.context("commit pre_seed tx")?;
        Ok(())
    }

    pub async fn conversation_states(&self) -> Result<HashMap<String, ConvState>> {
        let rows = sqlx::query(
            "SELECT id, updated_at, payload IS NOT NULL AS has_payload FROM conversations",
        )
        .fetch_all(&self.pool)
        .await
        .context("conversation_states")?;
        let mut out = HashMap::with_capacity(rows.len());
        for r in rows {
            let id: String = r.try_get("id").unwrap_or_default();
            let updated_at: Option<String> = r.try_get("updated_at").ok();
            let has: i64 = r.try_get("has_payload").unwrap_or(0);
            out.insert(
                id,
                ConvState {
                    updated_at,
                    has_payload: has != 0,
                },
            );
        }
        Ok(out)
    }

    pub async fn upsert_conversation_detail(&self, row: &ConversationDetail) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO conversations (id, org_uuid, name, updated_at, payload, fetched_at, last_attempt_at, last_error)
             VALUES (?, ?, ?, ?, ?, ?, ?, NULL)
             ON CONFLICT(id) DO UPDATE SET
                org_uuid = COALESCE(excluded.org_uuid, conversations.org_uuid),
                name = COALESCE(excluded.name, conversations.name),
                updated_at = COALESCE(excluded.updated_at, conversations.updated_at),
                payload = excluded.payload,
                fetched_at = excluded.fetched_at,
                last_attempt_at = excluded.last_attempt_at,
                last_error = NULL",
        )
        .bind(&row.id)
        .bind(&row.org_uuid)
        .bind(row.name.as_deref())
        .bind(row.updated_at.as_deref())
        .bind(&row.payload)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await
        .with_context(|| format!("upsert conversation {}", row.id))?;
        Ok(())
    }

    pub async fn record_conversation_error(&self, id: &str, err: &str) -> Result<()> {
        dr::record_object_error(&self.pool, "conversations", id, err).await
    }

    pub async fn failed_conversation_ids(&self) -> Result<Vec<String>> {
        dr::failed_ids(&self.pool, "conversations").await
    }

    pub async fn load_conversations(&self) -> Result<Vec<LoadedConversation>> {
        let rows = sqlx::query(
            "SELECT id, org_uuid, payload FROM conversations WHERE payload IS NOT NULL ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .context("load_conversations")?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let id: String = r.try_get("id").unwrap_or_default();
            let org_uuid: Option<String> = r.try_get("org_uuid").ok();
            let payload: String = match r.try_get("payload") {
                Ok(s) => s,
                Err(_) => continue,
            };
            let Ok(p) = serde_json::from_str::<Value>(&payload) else {
                continue;
            };
            out.push(LoadedConversation {
                id,
                org_uuid: org_uuid.unwrap_or_default(),
                payload: p,
            });
        }
        Ok(out)
    }

    // ── blobs (delegate) ───────────────────────────────────────────

    pub async fn blob_exists(&self, id: &str) -> Result<bool> {
        dr::blob_exists(&self.pool, id).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_blob_bytes(
        &self,
        id: &str,
        kind: &str,
        owning_id: &str,
        slot: &str,
        content_type: Option<&str>,
        bytes: &[u8],
        source_url: Option<&str>,
    ) -> Result<()> {
        dr::upsert_blob_bytes(
            &self.pool,
            id,
            kind,
            owning_id,
            slot,
            content_type,
            bytes,
            source_url,
        )
        .await
    }

    pub async fn record_blob_error(
        &self,
        id: &str,
        owning_id: &str,
        slot: &str,
        err: &str,
    ) -> Result<()> {
        dr::record_blob_error(&self.pool, id, owning_id, slot, err).await
    }

    pub async fn load_blobs_by_id(&self) -> Result<HashMap<String, BlobBytes>> {
        dr::load_blobs_by_id(&self.pool).await
    }
}

#[derive(Debug, Clone)]
pub struct LoadedConversation {
    pub id: String,
    pub org_uuid: String,
    /// Raw API payload — the translate step calls
    /// `normalize_to_export_shape` over this on read.
    pub payload: Value,
}

#[derive(Debug, Default, Clone)]
pub struct LoadedRaw {
    pub users: Vec<Value>,
    pub first_user_uuid: Option<String>,
    pub conversations: Vec<LoadedConversation>,
    pub blobs_by_id: HashMap<String, BlobBytes>,
}

pub fn block_on_load_all(db_path: &Path) -> Result<LoadedRaw> {
    let path = db_path.to_path_buf();
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let db = RawDb::open(&path).await?;
            Ok::<_, anyhow::Error>(LoadedRaw {
                users: db.load_users().await?,
                first_user_uuid: db.first_user_uuid().await?,
                conversations: db.load_conversations().await?,
                blobs_by_id: db.load_blobs_by_id().await?,
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn user_and_org_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("a.doltlite_db")).await.unwrap();
        db.upsert_user(&json!({"uuid": "u1", "email_address": "x@y", "full_name": "X"}))
            .await
            .unwrap();
        db.upsert_org(&json!({"uuid": "org-a", "name": "A Org"}))
            .await
            .unwrap();
        assert_eq!(db.first_user_uuid().await.unwrap(), Some("u1".into()));
    }

    #[tokio::test]
    async fn conversation_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("a.doltlite_db")).await.unwrap();
        db.upsert_conversation_detail(&ConversationDetail {
            id: "c1".into(),
            org_uuid: "org-a".into(),
            name: Some("Hi".into()),
            updated_at: Some("2026-01-01T00:00:00Z".into()),
            payload: serde_json::to_string(&json!({"uuid":"c1","chat_messages":[]})).unwrap(),
        })
        .await
        .unwrap();
        let convs = db.load_conversations().await.unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].org_uuid, "org-a");
    }
}
