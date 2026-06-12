//! Doltlite-backed raw store for the Anthropic (Claude) provider.
//!
//! Four tables — `users`, `orgs`, `conversations`,
//! `anthropic_attachments` — shared bookkeeping
//! (`<table>_bookkeeping`, `sync_runs`, …) lives in
//! [`frankweiler_etl::doltlite_raw`].
//!
//! Per the dolt_diff + per-provider CAS edge migration: attachment
//! bytes still ride in the shared `cas_objects`, but the (file_uuid →
//! blake3) mapping lives on `anthropic_attachments` rather than the
//! shared `blob_refs`. Conversation payloads are stored as the **raw**
//! `/api/...` response, post-normalization happening at read time in
//! `translate`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use frankweiler_etl::blob_cas::BlobCas;
use frankweiler_etl::doltlite_raw::{self as dr};

use super::schema_raw::{full_ddl, DATA_TABLES, MIGRATION_CONVERSATIONS_ADD_ORG_NAME};

pub use frankweiler_etl::doltlite_raw::db_path_for;

#[derive(Clone, Debug)]
pub struct RawDb {
    pool: SqlitePool,
    cas: BlobCas,
}

#[derive(Debug, Clone, Default)]
pub struct ConvState {
    pub updated_at: Option<String>,
    pub has_payload: bool,
}

impl RawDb {
    pub async fn open(db_path: &Path) -> Result<Self> {
        let owned = full_ddl();
        let slices: Vec<&str> = owned.iter().map(String::as_str).collect();
        let pool = dr::open(db_path, &slices).await?;
        // Idempotent migration for pre-org_name DBs.
        let _ = sqlx::query(MIGRATION_CONVERSATIONS_ADD_ORG_NAME)
            .execute(&pool)
            .await;
        let cas = BlobCas::open(&frankweiler_etl::blob_cas::cas_path_for(db_path)).await?;
        Ok(Self { pool, cas })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn cas(&self) -> &BlobCas {
        &self.cas
    }

    pub async fn reset(&self) -> Result<()> {
        dr::truncate_data_tables(&self.pool, DATA_TABLES).await
    }

    /// Replaces `truncate_blob_refs` for this provider.
    pub async fn clear_blob_hashes(&self) -> Result<()> {
        sqlx::query("UPDATE anthropic_attachments SET blake3 = NULL")
            .execute(&self.pool)
            .await
            .context("clear anthropic_attachments.blake3")?;
        Ok(())
    }

    // ── users ──────────────────────────────────────────────────────

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
    /// normalized conversations.
    pub async fn first_user_uuid(&self) -> Result<Option<String>> {
        let row = sqlx::query("SELECT id FROM users ORDER BY id LIMIT 1")
            .fetch_optional(&self.pool)
            .await
            .context("first_user_uuid")?;
        Ok(row.and_then(|r| r.try_get::<String, _>("id").ok()))
    }

    // ── conversations: listing pre-seed + skip-check ───────────────

    /// Pre-seed listing-derived rows. Existing rows keep their
    /// `payload` intact; only `updated_at` is refreshed. Lightweight
    /// SQL — keeps the surface predictable since these rows may not
    /// have a payload yet (so the bulk-upsert path with
    /// PAYLOAD_COLUMN = Some can't represent them).
    pub async fn pre_seed_conversations(&self, items: &[(&str, &Value)], now: &str) -> Result<()> {
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
            dr::record_object_attempt(&mut tx, "conversations", id, Some(now)).await?;
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

    pub async fn load_conversations(&self) -> Result<Vec<LoadedConversation>> {
        let rows = sqlx::query(
            "SELECT id, org_uuid, org_name, json(payload) AS payload FROM conversations \
              WHERE payload IS NOT NULL ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .context("load_conversations")?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let id: String = r.try_get("id").unwrap_or_default();
            let org_uuid: Option<String> = r.try_get("org_uuid").ok();
            let org_name: Option<String> = r.try_get("org_name").ok();
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
                org_name,
                payload: p,
            });
        }
        Ok(out)
    }

    /// `(file_uuid, blake3 IS NOT NULL)` lookup.
    pub async fn attachment_has_bytes(&self, file_uuid: &str) -> Result<bool> {
        let row = sqlx::query(
            "SELECT 1 FROM anthropic_attachments \
              WHERE file_uuid = ? AND blake3 IS NOT NULL LIMIT 1",
        )
        .bind(file_uuid)
        .fetch_optional(&self.pool)
        .await
        .with_context(|| format!("attachment_has_bytes {file_uuid}"))?;
        Ok(row.is_some())
    }
}

#[derive(Debug, Clone)]
pub struct LoadedConversation {
    pub id: String,
    pub org_uuid: String,
    pub org_name: Option<String>,
    pub payload: Value,
}

#[derive(Clone)]
pub struct LoadedRaw {
    pub users: Vec<Value>,
    pub first_user_uuid: Option<String>,
    pub conversations: Vec<LoadedConversation>,
    pub blobs: std::sync::Arc<dyn frankweiler_etl::blob_cas::BlobReader>,
}

impl Default for LoadedRaw {
    fn default() -> Self {
        Self {
            users: Vec::new(),
            first_user_uuid: None,
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
                std::sync::Arc::new(crate::translate::blob_reader::AnthropicBlobReader::new(
                    db.pool().clone(),
                    db.cas().pool().clone(),
                ));
            Ok::<_, anyhow::Error>(LoadedRaw {
                users: db.load_users().await?,
                first_user_uuid: db.first_user_uuid().await?,
                conversations: db.load_conversations().await?,
                blobs,
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extract::schema_raw::{OrgRow, UserRow};
    use frankweiler_etl::bulk::bulk_upsert_in_tx;
    use frankweiler_etl::doltlite_raw::WirePayload;
    use serde_json::json;

    const NOW: &str = "2026-06-11T00:00:00-07:00";

    fn make_user(id: &str, email: &str, name: &str) -> UserRow {
        UserRow {
            id_and_payload: WirePayload {
                id: id.into(),
                payload: serde_json::to_string(
                    &json!({"uuid": id, "email_address": email, "full_name": name}),
                )
                .unwrap(),
            },
            email: Some(email.into()),
            full_name: Some(name.into()),
        }
    }

    fn make_org(id: &str, name: &str) -> OrgRow {
        OrgRow {
            id_and_payload: WirePayload {
                id: id.into(),
                payload: serde_json::to_string(&json!({"uuid": id, "name": name})).unwrap(),
            },
            name: Some(name.into()),
        }
    }

    #[tokio::test]
    async fn user_and_org_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("a.doltlite_db")).await.unwrap();
        {
            let mut tx = db.pool().begin().await.unwrap();
            bulk_upsert_in_tx(&mut tx, &[make_user("u1", "x@y", "X")], NOW)
                .await
                .unwrap();
            bulk_upsert_in_tx(&mut tx, &[make_org("org-a", "A Org")], NOW)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
        assert_eq!(db.first_user_uuid().await.unwrap(), Some("u1".into()));
    }
}
