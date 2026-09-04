//! Doltlite-backed raw store for the Anthropic (Claude) provider.
//!
//! Six tables — `users`, `orgs`, `projects`, `project_docs`,
//! `conversations`, `anthropic_attachments` — shared bookkeeping
//! (`<table>_bookkeeping`, `sync_runs`, …) lives in
//! [`datalib_etl::doltlite_raw`].
//!
//! ## One reader per table
//!
//! Every table's read lives once, as a `*_from(&SqlitePool)` free
//! function; the [`RawDb`] methods are one-line delegations. Render
//! opens its own read-only pool (`render::parse`) and calls the same
//! free functions, so the download-side and render-side reads of a
//! table cannot drift — which they had, for `conversations` and
//! `first_user_uuid`, until this was made the rule.
//!
//! Per the dolt_diff + per-provider CAS edge migration: attachment
//! bytes still ride in the shared `cas_objects`, but the (file_uuid →
//! blake3) mapping lives on `anthropic_attachments` rather than the
//! shared `blob_refs`. Conversation payloads are stored as the **raw**
//! `/api/...` response, post-normalization happening at read time in
//! `render`.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use datalib_etl::blob_cas::BlobCas;
use datalib_etl::doltlite_raw::{self as dr};

use super::schema_raw::{full_ddl, DATA_TABLES, MIGRATION_CONVERSATIONS_ADD_ORG_NAME};

pub use datalib_etl::doltlite_raw::db_path_for;

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
        // Idempotent migration for pre-org_name DBs.
        let _ = sqlx::query(MIGRATION_CONVERSATIONS_ADD_ORG_NAME)
            .execute(&pool)
            .await;
        let cas = BlobCas::open(&datalib_etl::blob_cas::cas_path_for(db_path)).await?;
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

    /// Reset bytes-have-been-fetched state for `refetch_blobs`: clear
    /// the per-provider `blake3` column on `anthropic_attachments` so
    /// the next walk re-decodes and re-stores. The `(message_uuid,
    /// file_uuid)` edge metadata is upstream-driven so we leave the
    /// rows in place.
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

    /// Age of the most recent successful sweep for `key`.
    ///
    /// Same shape as slack's manifest-sweep marker (see
    /// `providers/slack/src/download/db.rs`): a row in the shared
    /// `sync_scope_state` table under a provider-namespaced `scope`, so no
    /// extra schema is needed. `None` when the sweep has never completed —
    /// which is what keeps a cold store doing the real call.
    pub async fn sweep_age(&self, key: &str) -> Result<Option<chrono::Duration>> {
        let scope = format!("anthropic:sweep:{key}");
        let row = sqlx::query("SELECT last_seen_at FROM sync_scope_state WHERE scope = ?")
            .bind(&scope)
            .fetch_optional(&self.pool)
            .await
            .context("select anthropic sweep marker")?;
        let Some(row) = row else { return Ok(None) };
        let s: String = row
            .try_get("last_seen_at")
            .context("read anthropic sweep timestamp")?;
        let dt = datalib_time::parse_strict(&s)
            .with_context(|| format!("parse anthropic sweep timestamp {s:?}"))?
            .inner()
            .with_timezone(&chrono::Utc);
        Ok(Some(chrono::Utc::now() - dt))
    }

    /// Stamp `key`'s sweep as completed at `now()`. Call only after the
    /// sweep's rows have been written, so an interrupted sweep doesn't
    /// poison the TTL check.
    pub async fn record_sweep(&self, key: &str) -> Result<()> {
        let scope = format!("anthropic:sweep:{key}");
        let now = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        sqlx::query(
            "INSERT INTO sync_scope_state (scope, last_seen_at) VALUES (?, ?) \
             ON CONFLICT(scope) DO UPDATE SET last_seen_at = excluded.last_seen_at",
        )
        .bind(&scope)
        .bind(&now)
        .execute(&self.pool)
        .await
        .context("record anthropic sweep marker")?;
        Ok(())
    }

    /// The `orgs` rows we already have, as raw payloads — what a warm
    /// [`Self::sweep_age`] hit serves instead of re-listing upstream.
    pub async fn load_orgs(&self) -> Result<Vec<Value>> {
        dr::load_payloads(&self.pool, "orgs").await
    }

    pub async fn load_users(&self) -> Result<Vec<Value>> {
        dr::load_payloads(&self.pool, "users").await
    }

    /// First user's uuid, used to fill the `account.uuid` field on
    /// normalized conversations.
    pub async fn first_user_uuid(&self) -> Result<Option<String>> {
        first_user_uuid_from(&self.pool).await
    }

    // ── conversations: listing skip-check ──────────────────────────

    /// Bulk-read `(id → updated_at)` for the listed ids. Returns one
    /// entry per *existing* row (with a non-null `updated_at`). Missing
    /// ids are absent from the map — caller treats them as "we don't
    /// have this conversation yet, fetch it." Used by the listing pass
    /// to decide which conversations need a detail fetch. Rows only
    /// exist post-detail-fetch, so "id in map" ↔ "payload present."
    pub async fn existing_updated_at(&self, ids: &[&str]) -> Result<HashMap<String, String>> {
        self.existing_updated_at_in("conversations", ids).await
    }

    // ── projects ───────────────────────────────────────────────────

    /// Bulk-read `(project id → updated_at)` for the listed ids, same
    /// shape and same purpose as [`Self::existing_updated_at`]: the
    /// caller compares against the live listing to decide which
    /// projects changed. Missing ids are absent from the map.
    pub async fn existing_project_updated_at(
        &self,
        ids: &[&str],
    ) -> Result<HashMap<String, String>> {
        self.existing_updated_at_in("projects", ids).await
    }

    /// Shared body of the two `existing_*_updated_at` skip-checks.
    async fn existing_updated_at_in(
        &self,
        table: &str,
        ids: &[&str],
    ) -> Result<HashMap<String, String>> {
        if ids.is_empty() {
            return Ok(HashMap::new());
        }
        let placeholders = std::iter::repeat_n("?", ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT id, updated_at FROM {table} \
              WHERE id IN ({placeholders}) AND updated_at IS NOT NULL"
        );
        // Audited: `table` is a literal at every callsite; `placeholders` is a
        // `?,?,?` run sized from `ids.len()` and each id is bound.
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for id in ids {
            q = q.bind(*id);
        }
        let rows = q
            .fetch_all(&self.pool)
            .await
            .with_context(|| format!("existing_updated_at {table}"))?;
        let mut out = HashMap::with_capacity(rows.len());
        for r in &rows {
            let id: String = r.try_get("id").unwrap_or_default();
            if let Ok(ut) = r.try_get::<String, _>("updated_at") {
                out.insert(id, ut);
            }
        }
        Ok(out)
    }

    /// Every stored project, with the org columns the render step needs
    /// to stamp on the project's grid rows.
    pub async fn load_projects(&self) -> Result<Vec<LoadedProject>> {
        load_projects_from(&self.pool).await
    }

    /// Every stored knowledge document, keyed by its owning project.
    pub async fn load_project_docs(&self) -> Result<Vec<LoadedProjectDoc>> {
        load_project_docs_from(&self.pool).await
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
        load_conversations_from(&self.pool).await
    }

    /// Snapshot `(file_uuid → blake3)` for every attachment whose
    /// bytes have ever landed in the CAS. Loaded once at the start of
    /// a fetch run; updated in-place as new downloads land. Replaces
    /// the per-file SQL `attachment_has_bytes` lookup.
    pub async fn load_attachment_blake3s(&self) -> Result<HashMap<String, String>> {
        datalib_etl::blob_cas::load_blake3_index(&self.pool, "anthropic_attachments", "file_uuid")
            .await
    }
}

/// One project as it sits between download and render.
#[derive(Debug, Clone)]
pub struct LoadedProject {
    pub id: String,
    pub org_uuid: Option<String>,
    pub org_name: Option<String>,
    pub payload: Value,
}

/// One knowledge document, with its owning project surfaced out of the
/// payload so render can bucket docs without re-parsing every one.
#[derive(Debug, Clone)]
pub struct LoadedProjectDoc {
    pub id: String,
    pub project_uuid: String,
    pub payload: Value,
}

/// Read `conversations` off any pool.
pub async fn load_conversations_from(pool: &SqlitePool) -> Result<Vec<LoadedConversation>> {
    let rows = sqlx::query(
        "SELECT id, org_uuid, org_name, json(payload) AS payload FROM conversations \
          WHERE payload IS NOT NULL ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .context("load_conversations")?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let Some(payload) = row_payload(r) else {
            continue;
        };
        out.push(LoadedConversation {
            id: r.try_get("id").unwrap_or_default(),
            org_uuid: r
                .try_get::<Option<String>, _>("org_uuid")
                .unwrap_or_default(),
            org_name: r.try_get("org_name").ok(),
            payload,
        });
    }
    Ok(out)
}

/// First user's uuid off any pool.
pub async fn first_user_uuid_from(pool: &SqlitePool) -> Result<Option<String>> {
    let row = sqlx::query("SELECT id FROM users ORDER BY id LIMIT 1")
        .fetch_optional(pool)
        .await
        .context("first_user_uuid")?;
    Ok(row.and_then(|r| r.try_get::<String, _>("id").ok()))
}

/// Read `projects` off any pool — the writable one [`RawDb`] holds and
/// the read-only one `render::parse` opens both go through here, so the
/// two can't drift.
pub async fn load_projects_from(pool: &SqlitePool) -> Result<Vec<LoadedProject>> {
    let rows = sqlx::query(
        "SELECT id, org_uuid, org_name, json(payload) AS payload FROM projects \
          WHERE payload IS NOT NULL ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .context("load_projects")?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let Some(payload) = row_payload(r) else {
            continue;
        };
        out.push(LoadedProject {
            id: r.try_get("id").unwrap_or_default(),
            org_uuid: r.try_get("org_uuid").ok(),
            org_name: r.try_get("org_name").ok(),
            payload,
        });
    }
    Ok(out)
}

/// Read `project_docs` off any pool. Rows whose `project_uuid` is null
/// are dropped: a doc with no owning project has nowhere to render.
pub async fn load_project_docs_from(pool: &SqlitePool) -> Result<Vec<LoadedProjectDoc>> {
    let rows = sqlx::query(
        "SELECT id, project_uuid, json(payload) AS payload FROM project_docs \
          WHERE payload IS NOT NULL AND project_uuid IS NOT NULL ORDER BY project_uuid, id",
    )
    .fetch_all(pool)
    .await
    .context("load_project_docs")?;
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        let Some(payload) = row_payload(r) else {
            continue;
        };
        let Ok(project_uuid) = r.try_get::<String, _>("project_uuid") else {
            continue;
        };
        out.push(LoadedProjectDoc {
            id: r.try_get("id").unwrap_or_default(),
            project_uuid,
            payload,
        });
    }
    Ok(out)
}

/// Decode the `json(payload)` column of a row, or `None` when it is
/// missing or unparseable. Skipping a corrupt row beats failing a whole
/// render over one.
fn row_payload(r: &sqlx::sqlite::SqliteRow) -> Option<Value> {
    let s: String = r.try_get("payload").ok()?;
    serde_json::from_str(&s).ok()
}

#[derive(Debug, Clone)]
pub struct LoadedConversation {
    pub id: String,
    /// Owning Anthropic organization, or `None`.
    ///
    /// **`None` is load-bearing**, not just missing data: only the live
    /// API walk learns an org (from `/organizations`), so a NULL column
    /// means this row was ingested from a bulk export by
    /// [`crate::download::export`] and its payload is therefore
    /// *already* in export shape. `render::parse::parse_loaded` keys
    /// its `normalize_to_export_shape` call off exactly that.
    pub org_uuid: Option<String>,
    pub org_name: Option<String>,
    pub payload: Value,
}

#[derive(Clone, Default)]
pub struct LoadedRaw {
    pub users: Vec<Value>,
    pub first_user_uuid: Option<String>,
    pub conversations: Vec<LoadedConversation>,
}

/// Synchronous helper for tests that want a snapshot of every entity
/// table at a fixed point in time. Production render uses
/// `crate::render::parse::parse(..., last_render_hash)` instead;
/// this one ignores the cursor and loads everything. Attachment bytes
/// are NOT loaded here — tests that need them load a `BlobBundle`
/// via `BlobBundle::load(...)` directly.
pub fn block_on_load_all(db_path: &Path) -> Result<LoadedRaw> {
    let path = db_path.to_path_buf();
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let db = RawDb::open(&path).await?;
            Ok::<_, anyhow::Error>(LoadedRaw {
                users: db.load_users().await?,
                first_user_uuid: db.first_user_uuid().await?,
                conversations: db.load_conversations().await?,
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::download::schema_raw::{OrgRow, UserRow};
    use datalib_etl::bulk::bulk_upsert_in_tx;
    use datalib_etl::doltlite_raw::WirePayload;
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

    /// A cold store has no marker, so `fetch` still makes the live
    /// `/organizations` call — which is what keeps that call working as a
    /// credential preflight on the run where a bad credential is likely.
    #[tokio::test]
    async fn sweep_age_is_none_before_any_sweep() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("a.doltlite_db")).await.unwrap();
        assert!(
            db.sweep_age("orgs").await.unwrap().is_none(),
            "a store that never completed a sweep must report no marker"
        );
    }

    /// After recording, the marker is fresh — so a warm store serves the
    /// stored rows instead of re-listing.
    #[tokio::test]
    async fn recorded_sweep_is_fresh_and_serves_stored_orgs() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("a.doltlite_db")).await.unwrap();
        {
            let mut tx = db.pool().begin().await.unwrap();
            bulk_upsert_in_tx(&mut tx, &[make_org("org-a", "A Org")], NOW)
                .await
                .unwrap();
            tx.commit().await.unwrap();
        }
        db.record_sweep("orgs").await.unwrap();

        let age = db
            .sweep_age("orgs")
            .await
            .unwrap()
            .expect("marker recorded");
        assert!(
            age < chrono::Duration::minutes(1),
            "a just-recorded sweep should be seconds old, got {age}"
        );
        assert!(
            age < super::super::ORGS_TTL,
            "a just-recorded sweep must be inside the TTL"
        );
        assert_eq!(
            db.load_orgs().await.unwrap().len(),
            1,
            "the warm path must be able to serve the stored orgs"
        );
    }

    /// Re-recording moves the marker rather than inserting a second row —
    /// the `ON CONFLICT` upsert. A duplicate would make `sweep_age`'s
    /// single-row read arbitrary.
    #[tokio::test]
    async fn record_sweep_is_idempotent() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("a.doltlite_db")).await.unwrap();
        db.record_sweep("orgs").await.unwrap();
        db.record_sweep("orgs").await.unwrap();
        let n: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM sync_scope_state WHERE scope = 'anthropic:sweep:orgs'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(n, 1, "expected exactly one marker row, got {n}");
    }

    /// The marker is namespaced per provider and per key, so it can share
    /// `sync_scope_state` with slack's markers and with the real resume
    /// cursors without collisions.
    #[tokio::test]
    async fn sweep_keys_are_namespaced() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("a.doltlite_db")).await.unwrap();
        db.record_sweep("orgs").await.unwrap();
        assert!(db.sweep_age("orgs").await.unwrap().is_some());
        assert!(
            db.sweep_age("something-else").await.unwrap().is_none(),
            "an unrelated key must not see the orgs marker"
        );
    }
}
