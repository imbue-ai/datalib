//! Doltlite-backed raw store for the GitLab provider.
//!
//! Replaces the event-store tree of `<entity>/{created,updated}/events.jsonl`
//! files with a single sqlite database at
//! `<data_root>/raw/<name>.doltlite_db`. Shared bookkeeping tables
//! (`blobs`, `endpoint_shapes`, `sync_runs`) and the open / blob
//! plumbing live in [`frankweiler_etl::doltlite_raw`]; the primary-key
//! policy that governs every object table here is documented there.
//!
//! Tables:
//! - `self_identity` — PK is the upstream GitLab user id (stringified).
//! - `merge_requests` — PK is `"<project_full_path>!<mr_iid>"`. Both
//!   parts are upstream-stable and known before the detail fetch
//!   (discovery surfaces them from the search results).
//! - `discussions` — PK is `"<project_full_path>!<mr_iid>#<discussion_id>"`.
//!   GitLab's discussion id is a hex string scoped to the project — we
//!   include the MR scope to keep the PK construction trivial and avoid
//!   surprises around bare discussion id collisions across projects.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::Value;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use frankweiler_etl::doltlite_raw::{self as dr};

pub use frankweiler_etl::doltlite_raw::db_path_for;

/// Data tables — what `dolt diff` should see across re-fetches.
/// Bookkeeping columns live in `<table>_bookkeeping` sidecars added
/// via `dr::bookkeeping_ddl_for(...)` below.
const DATA_TABLES: &[&str] = &["self_identity", "merge_requests", "discussions"];

const DDL_DATA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS self_identity (
        id TEXT PRIMARY KEY,
        username TEXT NULL,
        web_url TEXT NULL,
        payload TEXT NULL
    )",
    "CREATE TABLE IF NOT EXISTS merge_requests (
        id TEXT PRIMARY KEY,
        project_full_path TEXT NOT NULL,
        mr_iid INTEGER NOT NULL,
        state TEXT NULL,
        web_url TEXT NULL,
        head_sha TEXT NULL,
        base_sha TEXT NULL,
        start_sha TEXT NULL,
        source_branch TEXT NULL,
        target_branch TEXT NULL,
        updated_at TEXT NULL,
        merged_at TEXT NULL,
        payload TEXT NULL
    )",
    "CREATE INDEX IF NOT EXISTS merge_requests_by_proj ON merge_requests(project_full_path, mr_iid)",
    "CREATE TABLE IF NOT EXISTS discussions (
        id TEXT PRIMARY KEY,
        project_full_path TEXT NOT NULL,
        mr_iid INTEGER NOT NULL,
        discussion_id TEXT NOT NULL,
        individual_note INTEGER NULL,
        max_note_updated_at TEXT NULL,
        payload TEXT NULL
    )",
    "CREATE INDEX IF NOT EXISTS discussions_by_mr ON discussions(project_full_path, mr_iid)",
];

fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = DDL_DATA.iter().map(|s| (*s).to_string()).collect();
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}

pub fn mr_pk(proj: &str, iid: u32) -> String {
    format!("{proj}!{iid}")
}

pub fn discussion_pk(proj: &str, iid: u32, discussion_id: &str) -> String {
    format!("{proj}!{iid}#{discussion_id}")
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

    pub async fn start_run(&self, config: &Value) -> Result<i64> {
        dr::start_run(&self.pool, config).await
    }

    pub async fn finish_run(&self, run_id: i64, status: &str, summary: &Value) -> Result<()> {
        dr::finish_run(&self.pool, run_id, status, summary).await
    }

    // ── self_identity ───────────────────────────────────────────────

    pub async fn upsert_self_identity(&self, payload: &Value) -> Result<()> {
        let id = payload
            .get("id")
            .and_then(|v| v.as_i64())
            .map(|n| n.to_string())
            .ok_or_else(|| anyhow::anyhow!("/user response missing id"))?;
        let username = payload.get("username").and_then(|v| v.as_str());
        let web_url = payload.get("web_url").and_then(|v| v.as_str());
        let payload_str = serde_json::to_string(payload).context("serialize /user")?;
        let mut tx = self.pool.begin().await.context("begin self_identity tx")?;
        sqlx::query(
            "INSERT INTO self_identity (id, username, web_url, payload)
             VALUES (?, ?, ?, jsonb(?))
             ON CONFLICT(id) DO UPDATE SET
                username = COALESCE(excluded.username, self_identity.username),
                web_url = COALESCE(excluded.web_url, self_identity.web_url),
                payload = excluded.payload",
        )
        .bind(&id)
        .bind(username)
        .bind(web_url)
        .bind(&payload_str)
        .execute(&mut *tx)
        .await
        .context("upsert self_identity")?;
        dr::record_object_attempt(&mut tx, "self_identity", &id, None).await?;
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
        let id = mr_pk(proj, iid);
        let state = payload.get("state").and_then(|v| v.as_str());
        let web_url = payload.get("web_url").and_then(|v| v.as_str());
        let diff_refs = payload.get("diff_refs");
        let head_sha = diff_refs
            .and_then(|d| d.get("head_sha"))
            .and_then(|v| v.as_str());
        let base_sha = diff_refs
            .and_then(|d| d.get("base_sha"))
            .and_then(|v| v.as_str());
        let start_sha = diff_refs
            .and_then(|d| d.get("start_sha"))
            .and_then(|v| v.as_str());
        let source_branch = payload.get("source_branch").and_then(|v| v.as_str());
        let target_branch = payload.get("target_branch").and_then(|v| v.as_str());
        let updated_at = payload.get("updated_at").and_then(|v| v.as_str());
        let merged_at = payload.get("merged_at").and_then(|v| v.as_str());
        let payload_str = serde_json::to_string(payload).context("serialize MR")?;
        let mut tx = self.pool.begin().await.context("begin merge_request tx")?;
        sqlx::query(
            "INSERT INTO merge_requests
                (id, project_full_path, mr_iid, state, web_url, head_sha, base_sha, start_sha,
                 source_branch, target_branch, updated_at, merged_at, payload)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, jsonb(?))
             ON CONFLICT(id) DO UPDATE SET
                state = COALESCE(excluded.state, merge_requests.state),
                web_url = COALESCE(excluded.web_url, merge_requests.web_url),
                head_sha = COALESCE(excluded.head_sha, merge_requests.head_sha),
                base_sha = COALESCE(excluded.base_sha, merge_requests.base_sha),
                start_sha = COALESCE(excluded.start_sha, merge_requests.start_sha),
                source_branch = COALESCE(excluded.source_branch, merge_requests.source_branch),
                target_branch = COALESCE(excluded.target_branch, merge_requests.target_branch),
                updated_at = COALESCE(excluded.updated_at, merge_requests.updated_at),
                merged_at = COALESCE(excluded.merged_at, merge_requests.merged_at),
                payload = excluded.payload",
        )
        .bind(&id)
        .bind(proj)
        .bind(iid as i64)
        .bind(state)
        .bind(web_url)
        .bind(head_sha)
        .bind(base_sha)
        .bind(start_sha)
        .bind(source_branch)
        .bind(target_branch)
        .bind(updated_at)
        .bind(merged_at)
        .bind(&payload_str)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("upsert merge_request {id}"))?;
        dr::record_object_attempt(&mut tx, "merge_requests", &id, None).await?;
        tx.commit().await.context("commit merge_request tx")?;
        Ok(())
    }

    // ── discussions ─────────────────────────────────────────────────

    pub async fn upsert_discussion(&self, proj: &str, iid: u32, payload: &Value) -> Result<()> {
        let mut tx = self.pool.begin().await.context("begin discussion tx")?;
        let now = Utc::now().to_rfc3339();
        upsert_discussion_in(&mut tx, proj, iid, payload, &now).await?;
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
        let mut tx = self
            .pool
            .begin()
            .await
            .context("begin discussions batch tx")?;
        let now = Utc::now().to_rfc3339();
        for payload in payloads {
            upsert_discussion_in(&mut tx, proj, iid, payload, &now).await?;
        }
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

// ── private row-level upserts (shared by single + batch APIs) ──────────

async fn upsert_discussion_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    proj: &str,
    iid: u32,
    payload: &Value,
    _now: &str,
) -> Result<()> {
    let discussion_id = payload
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("discussion missing id"))?
        .to_string();
    let id = discussion_pk(proj, iid, &discussion_id);
    let individual_note = payload.get("individual_note").and_then(|v| v.as_bool());
    let max_note_updated_at = payload
        .get("notes")
        .and_then(|n| n.as_array())
        .and_then(|arr| {
            arr.iter()
                .filter_map(|n| n.get("updated_at").and_then(|v| v.as_str()))
                .max()
                .map(|s| s.to_string())
        });
    let payload_str = serde_json::to_string(payload).context("serialize discussion")?;
    sqlx::query(
        "INSERT INTO discussions
            (id, project_full_path, mr_iid, discussion_id, individual_note,
             max_note_updated_at, payload)
         VALUES (?, ?, ?, ?, ?, ?, jsonb(?))
         ON CONFLICT(id) DO UPDATE SET
            individual_note = COALESCE(excluded.individual_note, discussions.individual_note),
            max_note_updated_at = COALESCE(excluded.max_note_updated_at, discussions.max_note_updated_at),
            payload = excluded.payload",
    )
    .bind(&id)
    .bind(proj)
    .bind(iid as i64)
    .bind(&discussion_id)
    .bind(individual_note.map(|b| b as i64))
    .bind(max_note_updated_at.as_deref())
    .bind(&payload_str)
    .execute(&mut **tx)
    .await
    .with_context(|| format!("upsert discussion {id}"))?;
    dr::record_object_attempt(tx, "discussions", &id, None).await
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
