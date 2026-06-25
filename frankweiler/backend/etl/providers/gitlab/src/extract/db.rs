//! Doltlite-backed raw store for the GitLab provider.
//!
//! Replaces the event-store tree of `<entity>/{created,updated}/events.jsonl`
//! files with a single sqlite database at
//! `<data_root>/<name>/raw/entities.doltlite_db`. Shared bookkeeping tables
//! (`blobs`, `sync_runs`) and the open / blob plumbing live in
//! [`frankweiler_etl::doltlite_raw`]; the primary-key policy that
//! governs every object table here is documented there. The schema
//! itself — DDL constants, table list, PK recipes — lives in the
//! sibling [`super::schema_raw`] module; this file is the manipulation
//! layer.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use frankweiler_etl::bulk::bulk_upsert_in_tx;
use frankweiler_etl::doltlite_raw::{self as dr};

use super::schema_raw::{
    discussion_pk_recipe, full_ddl, mr_pk_recipe, DiscussionRow, MergeRequestRow, SelfIdentityRow,
    DATA_TABLES,
};

pub use frankweiler_etl::doltlite_raw::db_path_for;

pub fn mr_pk(proj: &str, iid: u32) -> String {
    mr_pk_recipe(proj, iid)
}

pub fn discussion_pk(proj: &str, iid: u32, discussion_id: &str) -> String {
    discussion_pk_recipe(proj, iid, discussion_id)
}

#[derive(Clone, Debug)]
pub struct RawDb {
    pool: SqlitePool,
}

impl RawDb {
    pub async fn open(db_path: &Path) -> Result<Self> {
        let owned = full_ddl();
        let slices: Vec<&str> = owned.iter().map(String::as_str).collect();
        let pool = dr::open(db_path, &slices).await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Wipe every per-row table so the next fetch re-downloads
    /// everything from upstream. See
    /// [`frankweiler_etl::doltlite_raw::truncate_data_tables`].
    pub async fn reset(&self) -> Result<()> {
        dr::truncate_data_tables(&self.pool, DATA_TABLES).await
    }

    // ── self_identity ───────────────────────────────────────────────

    pub async fn upsert_self_identity(&self, payload: &Value) -> Result<()> {
        let row = SelfIdentityRow::from_payload(payload)?;
        let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        let mut tx = self.pool.begin().await.context("begin self_identity tx")?;
        bulk_upsert_in_tx(&mut tx, &[row], &now).await?;
        tx.commit().await.context("commit self_identity tx")?;
        Ok(())
    }

    pub async fn load_self_identity(&self) -> Result<Option<Value>> {
        let row = sqlx::query(
            "SELECT json(payload) AS payload FROM self_identity \
             WHERE payload IS NOT NULL ORDER BY id LIMIT 1",
        )
        .fetch_optional(&self.pool)
        .await
        .context("select self_identity")?;
        let Some(row) = row else { return Ok(None) };
        let payload: Option<String> = row.try_get("payload").ok();
        Ok(payload.and_then(|s| serde_json::from_str(&s).ok()))
    }

    // ── merge_requests ──────────────────────────────────────────────

    pub async fn upsert_merge_request(&self, proj: &str, iid: u32, payload: &Value) -> Result<()> {
        let row = MergeRequestRow::from_payload(proj, iid, payload)?;
        let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        let mut tx = self.pool.begin().await.context("begin merge_request tx")?;
        bulk_upsert_in_tx(&mut tx, &[row], &now).await?;
        tx.commit().await.context("commit merge_request tx")?;
        Ok(())
    }

    // ── discussions ─────────────────────────────────────────────────

    pub async fn upsert_discussion(&self, proj: &str, iid: u32, payload: &Value) -> Result<()> {
        let row = DiscussionRow::from_payload(proj, iid, payload)?;
        let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        let mut tx = self.pool.begin().await.context("begin discussion tx")?;
        bulk_upsert_in_tx(&mut tx, &[row], &now).await?;
        tx.commit().await.context("commit discussion tx")?;
        Ok(())
    }

    /// Upsert every discussion of one MR in a single transaction. The
    /// natural commit boundary here is "all discussions for one MR" —
    /// the caller's outer loop is per-MR, and a partial set would just
    /// be re-fetched on the next sync.
    pub async fn upsert_discussions(&self, proj: &str, iid: u32, payloads: &[Value]) -> Result<()> {
        if payloads.is_empty() {
            return Ok(());
        }
        let rows: Vec<DiscussionRow> = payloads
            .iter()
            .map(|p| DiscussionRow::from_payload(proj, iid, p))
            .collect::<Result<Vec<_>>>()?;
        let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        let mut tx = self
            .pool
            .begin()
            .await
            .context("begin discussions batch tx")?;
        bulk_upsert_in_tx(&mut tx, &rows, &now).await?;
        tx.commit().await.context("commit discussions batch tx")?;
        Ok(())
    }

    // ── loads ───────────────────────────────────────────────────────

    pub async fn load_merge_requests(&self) -> Result<Vec<LoadedMergeRequest>> {
        let rows = sqlx::query(
            "SELECT id, project_full_path, mr_iid, json(payload) AS payload
             FROM merge_requests WHERE payload IS NOT NULL ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .context("select merge_requests")?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let payload_str: String = match r.try_get("payload") {
                Ok(s) => s,
                Err(_) => continue,
            };
            let Ok(payload) = serde_json::from_str::<Value>(&payload_str) else {
                continue;
            };
            out.push(LoadedMergeRequest {
                id: r.try_get("id").unwrap_or_default(),
                project_full_path: r.try_get("project_full_path").unwrap_or_default(),
                mr_iid: r.try_get::<i64, _>("mr_iid").unwrap_or(0) as u32,
                payload,
            });
        }
        Ok(out)
    }

    pub async fn load_discussions(&self) -> Result<Vec<LoadedDiscussion>> {
        let rows = sqlx::query(
            "SELECT id, project_full_path, mr_iid, discussion_id, json(payload) AS payload
             FROM discussions WHERE payload IS NOT NULL ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .context("select discussions")?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let payload_str: String = match r.try_get("payload") {
                Ok(s) => s,
                Err(_) => continue,
            };
            let Ok(payload) = serde_json::from_str::<Value>(&payload_str) else {
                continue;
            };
            out.push(LoadedDiscussion {
                id: r.try_get("id").unwrap_or_default(),
                project_full_path: r.try_get("project_full_path").unwrap_or_default(),
                mr_iid: r.try_get::<i64, _>("mr_iid").unwrap_or(0) as u32,
                discussion_id: r.try_get("discussion_id").unwrap_or_default(),
                payload,
            });
        }
        Ok(out)
    }

    // ── sync_scope_state (delegates) ────────────────────────────────

    pub async fn load_scope_state(&self) -> Result<HashMap<String, String>> {
        dr::load_scope_state(&self.pool).await
    }

    pub async fn upsert_scope_state(&self, scope: &str, last_seen_at: &str) -> Result<()> {
        dr::upsert_scope_state(&self.pool, scope, last_seen_at).await
    }

    pub async fn any_merge_requests(&self) -> Result<bool> {
        let row = sqlx::query("SELECT 1 FROM merge_requests WHERE payload IS NOT NULL LIMIT 1")
            .fetch_optional(&self.pool)
            .await
            .context("any_merge_requests")?;
        Ok(row.is_some())
    }

    /// Return `(proj, iid) → updated_at` for every MR we already have
    /// the full payload for. Used by the extract loop to skip detail
    /// fetches when the listing's `updated_at` matches what's on disk.
    pub async fn merge_request_updated_ats(&self) -> Result<HashMap<(String, u32), String>> {
        let rows = sqlx::query(
            "SELECT project_full_path, mr_iid, updated_at
             FROM merge_requests
             WHERE payload IS NOT NULL AND updated_at IS NOT NULL",
        )
        .fetch_all(&self.pool)
        .await
        .context("merge_request_updated_ats")?;
        let mut out: HashMap<(String, u32), String> = HashMap::with_capacity(rows.len());
        for r in rows {
            let proj: String = r.get("project_full_path");
            let iid_i64: i64 = r.get("mr_iid");
            let updated_at: String = r.get("updated_at");
            out.insert((proj, iid_i64 as u32), updated_at);
        }
        Ok(out)
    }
}

#[derive(Debug, Clone)]
pub struct LoadedMergeRequest {
    pub id: String,
    pub project_full_path: String,
    pub mr_iid: u32,
    pub payload: Value,
}

#[derive(Debug, Clone)]
pub struct LoadedDiscussion {
    pub id: String,
    pub project_full_path: String,
    pub mr_iid: u32,
    pub discussion_id: String,
    pub payload: Value,
}

#[derive(Clone, Default)]
pub struct LoadedRaw {
    pub self_identity: Option<Value>,
    pub merge_requests: Vec<LoadedMergeRequest>,
    pub discussions: Vec<LoadedDiscussion>,
}

pub fn block_on_load_all(db_path: &Path) -> Result<LoadedRaw> {
    let path = db_path.to_path_buf();
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let db = RawDb::open(&path).await?;
            Ok::<_, anyhow::Error>(LoadedRaw {
                self_identity: db.load_self_identity().await?,
                merge_requests: db.load_merge_requests().await?,
                discussions: db.load_discussions().await?,
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn self_identity_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("g.doltlite_db")).await.unwrap();
        db.upsert_self_identity(
            &json!({"id": 7, "username": "tt", "web_url": "https://gitlab.com/tt"}),
        )
        .await
        .unwrap();
        let me = db.load_self_identity().await.unwrap().expect("self");
        assert_eq!(me["id"], 7);
        assert_eq!(me["username"], "tt");
    }

    #[tokio::test]
    async fn mr_and_discussion_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("g.doltlite_db")).await.unwrap();
        db.upsert_merge_request(
            "ns/proj",
            12,
            &json!({
                "iid": 12,
                "web_url": "https://gitlab.com/ns/proj/-/merge_requests/12",
                "state": "opened",
                "source_branch": "feat",
                "target_branch": "main",
            }),
        )
        .await
        .unwrap();
        db.upsert_discussion(
            "ns/proj",
            12,
            &json!({"id": "abc", "individual_note": false, "notes": [{"updated_at": "2025-01-01T00:00:00Z"}]}),
        )
        .await
        .unwrap();
        let mrs = db.load_merge_requests().await.unwrap();
        assert_eq!(mrs.len(), 1);
        assert_eq!(mrs[0].mr_iid, 12);
        let ds = db.load_discussions().await.unwrap();
        assert_eq!(ds.len(), 1);
        assert_eq!(ds[0].discussion_id, "abc");
    }

    #[tokio::test]
    async fn payload_stored_as_jsonb_blob() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("g.doltlite_db")).await.unwrap();
        db.upsert_merge_request("ns/proj", 12, &json!({"iid": 12, "state": "opened"}))
            .await
            .unwrap();
        let row =
            sqlx::query("SELECT typeof(payload) AS t FROM merge_requests WHERE id='ns/proj!12'")
                .fetch_one(db.pool())
                .await
                .unwrap();
        let t: String = row.try_get("t").unwrap();
        assert_eq!(t, "blob", "payload should be JSONB-encoded BLOB");
    }
}
