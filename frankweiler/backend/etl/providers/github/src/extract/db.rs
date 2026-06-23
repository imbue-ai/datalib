//! Doltlite-backed raw store for the GitHub provider.
//!
//! Replaces the event-store tree of `<entity>/{created,updated}/events.jsonl`
//! files with a single sqlite database at
//! `<data_root>/raw/<name>/entities.doltlite_db`. Shared bookkeeping tables
//! (`blobs`, `sync_runs`) and the open / blob
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

use frankweiler_etl::bulk::bulk_upsert_in_tx;
use frankweiler_etl::doltlite_raw::{self as dr};

pub use frankweiler_etl::doltlite_raw::db_path_for;

use super::schema_raw::{
    full_ddl, IssueCommentRow, PrReviewCommentRow, PrReviewRow, PullRequestRow, SelfIdentityRow,
    DATA_TABLES,
};

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

    // ── pull_requests ───────────────────────────────────────────────

    pub async fn upsert_pull_request(&self, repo: &str, num: u32, payload: &Value) -> Result<()> {
        let row = PullRequestRow::from_payload(repo, num, payload)?;
        let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        let mut tx = self.pool.begin().await.context("begin pull_request tx")?;
        bulk_upsert_in_tx(&mut tx, &[row], &now).await?;
        tx.commit().await.context("commit pull_request tx")?;
        Ok(())
    }

    // ── issue_comments / pr_reviews / pr_review_comments ────────────

    pub async fn upsert_issue_comment(&self, repo: &str, num: u32, payload: &Value) -> Result<()> {
        let row = IssueCommentRow::from_payload(repo, num, payload)?;
        let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        let mut tx = self.pool.begin().await.context("begin issue_comment tx")?;
        bulk_upsert_in_tx(&mut tx, &[row], &now).await?;
        tx.commit().await.context("commit issue_comment tx")?;
        Ok(())
    }

    pub async fn upsert_pr_review(&self, repo: &str, num: u32, payload: &Value) -> Result<()> {
        let row = PrReviewRow::from_payload(repo, num, payload)?;
        let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        let mut tx = self.pool.begin().await.context("begin pr_review tx")?;
        bulk_upsert_in_tx(&mut tx, &[row], &now).await?;
        tx.commit().await.context("commit pr_review tx")?;
        Ok(())
    }

    pub async fn upsert_pr_review_comment(
        &self,
        repo: &str,
        num: u32,
        payload: &Value,
    ) -> Result<()> {
        let row = PrReviewCommentRow::from_payload(repo, num, payload)?;
        let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        let mut tx = self
            .pool
            .begin()
            .await
            .context("begin pr_review_comment tx")?;
        bulk_upsert_in_tx(&mut tx, &[row], &now).await?;
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
