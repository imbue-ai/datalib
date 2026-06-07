//! Doltlite-backed raw store for the GitHub provider.
//!
//! Replaces the event-store tree of `<entity>/{created,updated}/events.jsonl`
//! files with a single sqlite database at
//! `<data_root>/raw/<name>.doltlite_db`. Shared bookkeeping tables
//! (`blobs`, `endpoint_shapes`, `sync_runs`) and the open / blob
//! plumbing live in [`frankweiler_etl::doltlite_raw`]; the primary-key
//! policy that governs every object table here is documented there.
//!
//! Tables:
//! - `self_identity` — PK is the upstream user id (numeric, stringified).
//!   Single-row identity capture from `GET /user`.
//! - `pull_requests` — PK is `"<repo_full_name>#<pr_number>"`. The
//!   composite key is upstream-stable and known before the detail fetch
//!   (discovery surfaces it from the search results).
//! - `issue_comments` / `pr_reviews` / `pr_review_comments` — PK is the
//!   stringified GitHub-global numeric id. Those id spaces are disjoint
//!   per endpoint, so no namespacing prefix is needed.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use frankweiler_etl::doltlite_raw::{self as dr};

pub use frankweiler_etl::doltlite_raw::db_path_for;

/// Data tables — what `dolt diff` should see across re-fetches.
/// Bookkeeping columns live in `<table>_bookkeeping` sidecars added
/// via `dr::bookkeeping_ddl_for(...)` below.
const DATA_TABLES: &[&str] = &[
    "self_identity",
    "pull_requests",
    "issue_comments",
    "pr_reviews",
    "pr_review_comments",
];

const DDL_DATA: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS self_identity (
        id TEXT PRIMARY KEY,
        login TEXT NULL,
        html_url TEXT NULL,
        payload TEXT NULL
    )",
    "CREATE TABLE IF NOT EXISTS pull_requests (
        id TEXT PRIMARY KEY,
        repo_full_name TEXT NOT NULL,
        pr_number INTEGER NOT NULL,
        state TEXT NULL,
        html_url TEXT NULL,
        head_sha TEXT NULL,
        base_sha TEXT NULL,
        head_ref TEXT NULL,
        base_ref TEXT NULL,
        updated_at TEXT NULL,
        merged_at TEXT NULL,
        payload TEXT NULL
    )",
    "CREATE INDEX IF NOT EXISTS pull_requests_by_repo ON pull_requests(repo_full_name, pr_number)",
    "CREATE TABLE IF NOT EXISTS issue_comments (
        id TEXT PRIMARY KEY,
        repo_full_name TEXT NOT NULL,
        pr_number INTEGER NOT NULL,
        html_url TEXT NULL,
        user_login TEXT NULL,
        created_at TEXT NULL,
        updated_at TEXT NULL,
        payload TEXT NULL
    )",
    "CREATE INDEX IF NOT EXISTS issue_comments_by_pr ON issue_comments(repo_full_name, pr_number)",
    "CREATE TABLE IF NOT EXISTS pr_reviews (
        id TEXT PRIMARY KEY,
        repo_full_name TEXT NOT NULL,
        pr_number INTEGER NOT NULL,
        state TEXT NULL,
        commit_id TEXT NULL,
        user_login TEXT NULL,
        submitted_at TEXT NULL,
        html_url TEXT NULL,
        payload TEXT NULL
    )",
    "CREATE INDEX IF NOT EXISTS pr_reviews_by_pr ON pr_reviews(repo_full_name, pr_number)",
    "CREATE TABLE IF NOT EXISTS pr_review_comments (
        id TEXT PRIMARY KEY,
        repo_full_name TEXT NOT NULL,
        pr_number INTEGER NOT NULL,
        in_reply_to_id INTEGER NULL,
        pull_request_review_id INTEGER NULL,
        html_url TEXT NULL,
        user_login TEXT NULL,
        path TEXT NULL,
        line INTEGER NULL,
        original_line INTEGER NULL,
        commit_id TEXT NULL,
        original_commit_id TEXT NULL,
        created_at TEXT NULL,
        updated_at TEXT NULL,
        payload TEXT NULL
    )",
    "CREATE INDEX IF NOT EXISTS pr_review_comments_by_pr ON pr_review_comments(repo_full_name, pr_number)",
];

fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = DDL_DATA.iter().map(|s| (*s).to_string()).collect();
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}

/// Composite PK for a PR row: `"<repo>#<num>"`.
pub fn pr_pk(repo: &str, num: u32) -> String {
    format!("{repo}#{num}")
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
        let id = payload
            .get("id")
            .and_then(|v| v.as_i64())
            .map(|n| n.to_string())
            .ok_or_else(|| anyhow::anyhow!("/user response missing id"))?;
        let login = payload.get("login").and_then(|v| v.as_str());
        let html_url = payload.get("html_url").and_then(|v| v.as_str());
        let payload_str = serde_json::to_string(payload).context("serialize /user")?;
        let mut tx = self.pool.begin().await.context("begin self_identity tx")?;
        sqlx::query(
            "INSERT INTO self_identity (id, login, html_url, payload)
             VALUES (?, ?, ?, jsonb(?))
             ON CONFLICT(id) DO UPDATE SET
                login = COALESCE(excluded.login, self_identity.login),
                html_url = COALESCE(excluded.html_url, self_identity.html_url),
                payload = excluded.payload",
        )
        .bind(&id)
        .bind(login)
        .bind(html_url)
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

    // ── pull_requests ───────────────────────────────────────────────

    pub async fn upsert_pull_request(&self, repo: &str, num: u32, payload: &Value) -> Result<()> {
        let id = pr_pk(repo, num);
        let state = payload.get("state").and_then(|v| v.as_str());
        let html_url = payload.get("html_url").and_then(|v| v.as_str());
        let head = payload.get("head");
        let base = payload.get("base");
        let head_sha = head.and_then(|h| h.get("sha")).and_then(|v| v.as_str());
        let head_ref = head.and_then(|h| h.get("ref")).and_then(|v| v.as_str());
        let base_sha = base.and_then(|b| b.get("sha")).and_then(|v| v.as_str());
        let base_ref = base.and_then(|b| b.get("ref")).and_then(|v| v.as_str());
        let updated_at = payload.get("updated_at").and_then(|v| v.as_str());
        let merged_at = payload.get("merged_at").and_then(|v| v.as_str());
        let payload_str = serde_json::to_string(payload).context("serialize PR")?;
        let mut tx = self.pool.begin().await.context("begin pull_request tx")?;
        sqlx::query(
            "INSERT INTO pull_requests
                (id, repo_full_name, pr_number, state, html_url, head_sha, base_sha,
                 head_ref, base_ref, updated_at, merged_at, payload)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, jsonb(?))
             ON CONFLICT(id) DO UPDATE SET
                state = COALESCE(excluded.state, pull_requests.state),
                html_url = COALESCE(excluded.html_url, pull_requests.html_url),
                head_sha = COALESCE(excluded.head_sha, pull_requests.head_sha),
                base_sha = COALESCE(excluded.base_sha, pull_requests.base_sha),
                head_ref = COALESCE(excluded.head_ref, pull_requests.head_ref),
                base_ref = COALESCE(excluded.base_ref, pull_requests.base_ref),
                updated_at = COALESCE(excluded.updated_at, pull_requests.updated_at),
                merged_at = COALESCE(excluded.merged_at, pull_requests.merged_at),
                payload = excluded.payload",
        )
        .bind(&id)
        .bind(repo)
        .bind(num as i64)
        .bind(state)
        .bind(html_url)
        .bind(head_sha)
        .bind(base_sha)
        .bind(head_ref)
        .bind(base_ref)
        .bind(updated_at)
        .bind(merged_at)
        .bind(&payload_str)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("upsert pull_request {id}"))?;
        dr::record_object_attempt(&mut tx, "pull_requests", &id, None).await?;
        tx.commit().await.context("commit pull_request tx")?;
        Ok(())
    }

    // ── issue_comments / pr_reviews / pr_review_comments ────────────

    pub async fn upsert_issue_comment(&self, repo: &str, num: u32, payload: &Value) -> Result<()> {
        let id = payload
            .get("id")
            .and_then(|v| v.as_i64())
            .map(|n| n.to_string())
            .ok_or_else(|| anyhow::anyhow!("issue_comment missing id"))?;
        let html_url = payload.get("html_url").and_then(|v| v.as_str());
        let user_login = payload
            .get("user")
            .and_then(|u| u.get("login"))
            .and_then(|v| v.as_str());
        let created_at = payload.get("created_at").and_then(|v| v.as_str());
        let updated_at = payload.get("updated_at").and_then(|v| v.as_str());
        let payload_str = serde_json::to_string(payload).context("serialize issue_comment")?;
        let mut tx = self.pool.begin().await.context("begin issue_comment tx")?;
        sqlx::query(
            "INSERT INTO issue_comments
                (id, repo_full_name, pr_number, html_url, user_login, created_at, updated_at,
                 payload)
             VALUES (?, ?, ?, ?, ?, ?, ?, jsonb(?))
             ON CONFLICT(id) DO UPDATE SET
                html_url = COALESCE(excluded.html_url, issue_comments.html_url),
                user_login = COALESCE(excluded.user_login, issue_comments.user_login),
                created_at = COALESCE(excluded.created_at, issue_comments.created_at),
                updated_at = COALESCE(excluded.updated_at, issue_comments.updated_at),
                payload = excluded.payload",
        )
        .bind(&id)
        .bind(repo)
        .bind(num as i64)
        .bind(html_url)
        .bind(user_login)
        .bind(created_at)
        .bind(updated_at)
        .bind(&payload_str)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("upsert issue_comment {id}"))?;
        dr::record_object_attempt(&mut tx, "issue_comments", &id, None).await?;
        tx.commit().await.context("commit issue_comment tx")?;
        Ok(())
    }

    pub async fn upsert_pr_review(&self, repo: &str, num: u32, payload: &Value) -> Result<()> {
        let id = payload
            .get("id")
            .and_then(|v| v.as_i64())
            .map(|n| n.to_string())
            .ok_or_else(|| anyhow::anyhow!("pr_review missing id"))?;
        let state = payload.get("state").and_then(|v| v.as_str());
        let commit_id = payload.get("commit_id").and_then(|v| v.as_str());
        let user_login = payload
            .get("user")
            .and_then(|u| u.get("login"))
            .and_then(|v| v.as_str());
        let submitted_at = payload.get("submitted_at").and_then(|v| v.as_str());
        let html_url = payload.get("html_url").and_then(|v| v.as_str());
        let payload_str = serde_json::to_string(payload).context("serialize pr_review")?;
        let mut tx = self.pool.begin().await.context("begin pr_review tx")?;
        sqlx::query(
            "INSERT INTO pr_reviews
                (id, repo_full_name, pr_number, state, commit_id, user_login, submitted_at,
                 html_url, payload)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, jsonb(?))
             ON CONFLICT(id) DO UPDATE SET
                state = COALESCE(excluded.state, pr_reviews.state),
                commit_id = COALESCE(excluded.commit_id, pr_reviews.commit_id),
                user_login = COALESCE(excluded.user_login, pr_reviews.user_login),
                submitted_at = COALESCE(excluded.submitted_at, pr_reviews.submitted_at),
                html_url = COALESCE(excluded.html_url, pr_reviews.html_url),
                payload = excluded.payload",
        )
        .bind(&id)
        .bind(repo)
        .bind(num as i64)
        .bind(state)
        .bind(commit_id)
        .bind(user_login)
        .bind(submitted_at)
        .bind(html_url)
        .bind(&payload_str)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("upsert pr_review {id}"))?;
        dr::record_object_attempt(&mut tx, "pr_reviews", &id, None).await?;
        tx.commit().await.context("commit pr_review tx")?;
        Ok(())
    }

    pub async fn upsert_pr_review_comment(
        &self,
        repo: &str,
        num: u32,
        payload: &Value,
    ) -> Result<()> {
        let id = payload
            .get("id")
            .and_then(|v| v.as_i64())
            .map(|n| n.to_string())
            .ok_or_else(|| anyhow::anyhow!("pr_review_comment missing id"))?;
        let in_reply_to_id = payload.get("in_reply_to_id").and_then(|v| v.as_i64());
        let pull_request_review_id = payload
            .get("pull_request_review_id")
            .and_then(|v| v.as_i64());
        let html_url = payload.get("html_url").and_then(|v| v.as_str());
        let user_login = payload
            .get("user")
            .and_then(|u| u.get("login"))
            .and_then(|v| v.as_str());
        let path = payload.get("path").and_then(|v| v.as_str());
        let line = payload.get("line").and_then(|v| v.as_i64());
        let original_line = payload.get("original_line").and_then(|v| v.as_i64());
        let commit_id = payload.get("commit_id").and_then(|v| v.as_str());
        let original_commit_id = payload.get("original_commit_id").and_then(|v| v.as_str());
        let created_at = payload.get("created_at").and_then(|v| v.as_str());
        let updated_at = payload.get("updated_at").and_then(|v| v.as_str());
        let payload_str = serde_json::to_string(payload).context("serialize pr_review_comment")?;
        let mut tx = self
            .pool
            .begin()
            .await
            .context("begin pr_review_comment tx")?;
        sqlx::query(
            "INSERT INTO pr_review_comments
                (id, repo_full_name, pr_number, in_reply_to_id, pull_request_review_id,
                 html_url, user_login, path, line, original_line, commit_id, original_commit_id,
                 created_at, updated_at, payload)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, jsonb(?))
             ON CONFLICT(id) DO UPDATE SET
                in_reply_to_id = COALESCE(excluded.in_reply_to_id, pr_review_comments.in_reply_to_id),
                pull_request_review_id = COALESCE(excluded.pull_request_review_id, pr_review_comments.pull_request_review_id),
                html_url = COALESCE(excluded.html_url, pr_review_comments.html_url),
                user_login = COALESCE(excluded.user_login, pr_review_comments.user_login),
                path = COALESCE(excluded.path, pr_review_comments.path),
                line = COALESCE(excluded.line, pr_review_comments.line),
                original_line = COALESCE(excluded.original_line, pr_review_comments.original_line),
                commit_id = COALESCE(excluded.commit_id, pr_review_comments.commit_id),
                original_commit_id = COALESCE(excluded.original_commit_id, pr_review_comments.original_commit_id),
                created_at = COALESCE(excluded.created_at, pr_review_comments.created_at),
                updated_at = COALESCE(excluded.updated_at, pr_review_comments.updated_at),
                payload = excluded.payload",
        )
        .bind(&id)
        .bind(repo)
        .bind(num as i64)
        .bind(in_reply_to_id)
        .bind(pull_request_review_id)
        .bind(html_url)
        .bind(user_login)
        .bind(path)
        .bind(line)
        .bind(original_line)
        .bind(commit_id)
        .bind(original_commit_id)
        .bind(created_at)
        .bind(updated_at)
        .bind(&payload_str)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("upsert pr_review_comment {id}"))?;
        dr::record_object_attempt(&mut tx, "pr_review_comments", &id, None).await?;
        tx.commit().await.context("commit pr_review_comment tx")?;
        Ok(())
    }

    // ── loads ───────────────────────────────────────────────────────

    pub async fn load_pull_requests(&self) -> Result<Vec<LoadedPullRequest>> {
        let rows = sqlx::query(
            "SELECT id, repo_full_name, pr_number, json(payload) AS payload
             FROM pull_requests WHERE payload IS NOT NULL ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .context("select pull_requests")?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let payload_str: String = match r.try_get("payload") {
                Ok(s) => s,
                Err(_) => continue,
            };
            let Ok(payload) = serde_json::from_str::<Value>(&payload_str) else {
                continue;
            };
            out.push(LoadedPullRequest {
                id: r.try_get("id").unwrap_or_default(),
                repo_full_name: r.try_get("repo_full_name").unwrap_or_default(),
                pr_number: r.try_get::<i64, _>("pr_number").unwrap_or(0) as u32,
                payload,
            });
        }
        Ok(out)
    }

    pub async fn load_children(&self, table: &str) -> Result<Vec<LoadedChild>> {
        // `table` is a static identifier supplied by us, not user input —
        // safe to interpolate.
        let sql = format!(
            "SELECT id, repo_full_name, pr_number, json(payload) AS payload
             FROM {table} WHERE payload IS NOT NULL ORDER BY id"
        );
        let rows = sqlx::query(&sql)
            .fetch_all(&self.pool)
            .await
            .with_context(|| format!("select {table}"))?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let payload_str: String = match r.try_get("payload") {
                Ok(s) => s,
                Err(_) => continue,
            };
            let Ok(payload) = serde_json::from_str::<Value>(&payload_str) else {
                continue;
            };
            out.push(LoadedChild {
                id: r.try_get("id").unwrap_or_default(),
                repo_full_name: r.try_get("repo_full_name").unwrap_or_default(),
                pr_number: r.try_get::<i64, _>("pr_number").unwrap_or(0) as u32,
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

    pub async fn any_pull_requests(&self) -> Result<bool> {
        let row = sqlx::query("SELECT 1 FROM pull_requests WHERE payload IS NOT NULL LIMIT 1")
            .fetch_optional(&self.pool)
            .await
            .context("any_pull_requests")?;
        Ok(row.is_some())
    }
}

/// One PR row loaded back out of the DB. Carries the upstream-stable
/// composite id (`<repo>#<num>`), the promoted scoping columns, and the
/// raw payload that the translate layer parses.
#[derive(Debug, Clone)]
pub struct LoadedPullRequest {
    pub id: String,
    pub repo_full_name: String,
    pub pr_number: u32,
    pub payload: Value,
}

/// One PR-child row (issue_comment / pr_review / pr_review_comment) loaded
/// back out of the DB. Same shape across the three child tables.
#[derive(Debug, Clone)]
pub struct LoadedChild {
    pub id: String,
    pub repo_full_name: String,
    pub pr_number: u32,
    pub payload: Value,
}

/// Bag returned to the synchronous translate path. GitHub doesn't ship
/// any binary blobs, so there's no blob handle here.
#[derive(Clone, Default)]
pub struct LoadedRaw {
    pub self_identity: Option<Value>,
    pub pull_requests: Vec<LoadedPullRequest>,
    pub issue_comments: Vec<LoadedChild>,
    pub pr_reviews: Vec<LoadedChild>,
    pub pr_review_comments: Vec<LoadedChild>,
}

/// Synchronous helper for non-async callers (translate, synthesize) that
/// already run under `#[tokio::main]`. Uses `block_in_place` + the
/// current Handle, so it must be invoked on a multi-thread runtime.
pub fn block_on_load_all(db_path: &Path) -> Result<LoadedRaw> {
    let path = db_path.to_path_buf();
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let db = RawDb::open(&path).await?;
            Ok::<_, anyhow::Error>(LoadedRaw {
                self_identity: db.load_self_identity().await?,
                pull_requests: db.load_pull_requests().await?,
                issue_comments: db.load_children("issue_comments").await?,
                pr_reviews: db.load_children("pr_reviews").await?,
                pr_review_comments: db.load_children("pr_review_comments").await?,
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
        db.upsert_self_identity(&json!({"id": 42, "login": "octocat"}))
            .await
            .unwrap();
        let me = db.load_self_identity().await.unwrap().expect("self");
        assert_eq!(me["id"], 42);
        assert_eq!(me["login"], "octocat");
    }

    #[tokio::test]
    async fn pr_and_children_round_trip() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("g.doltlite_db")).await.unwrap();
        db.upsert_pull_request(
            "octocat/hello",
            7,
            &json!({
                "number": 7,
                "title": "T",
                "state": "open",
                "head": {"sha": "abc", "ref": "br"},
                "base": {"sha": "def", "ref": "main"},
            }),
        )
        .await
        .unwrap();
        db.upsert_issue_comment(
            "octocat/hello",
            7,
            &json!({"id": 101, "body": "hi", "user": {"login": "alice"}}),
        )
        .await
        .unwrap();
        let prs = db.load_pull_requests().await.unwrap();
        assert_eq!(prs.len(), 1);
        assert_eq!(prs[0].pr_number, 7);
        let ics = db.load_children("issue_comments").await.unwrap();
        assert_eq!(ics.len(), 1);
        assert_eq!(ics[0].id, "101");
    }

    #[tokio::test]
    async fn payload_stored_as_jsonb_blob() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("g.doltlite_db")).await.unwrap();
        db.upsert_pull_request("octocat/hello", 7, &json!({"number": 7, "title": "T"}))
            .await
            .unwrap();
        let row = sqlx::query(
            "SELECT typeof(payload) AS t FROM pull_requests WHERE id='octocat/hello#7'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        let t: String = row.try_get("t").unwrap();
        assert_eq!(t, "blob", "payload should be JSONB-encoded BLOB");
    }

    #[tokio::test]
    async fn scope_state_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("g.doltlite_db")).await.unwrap();
        db.upsert_scope_state("author:@me", "2026-05-21T00:00:00Z")
            .await
            .unwrap();
        let m = db.load_scope_state().await.unwrap();
        assert_eq!(
            m.get("author:@me").map(String::as_str),
            Some("2026-05-21T00:00:00Z")
        );
    }
}
