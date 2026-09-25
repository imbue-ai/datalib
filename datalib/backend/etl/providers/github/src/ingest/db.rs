//! Doltlite-backed raw store for the GitHub provider.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::Row;

use datalib_etl::bulk::bulk_upsert;
use datalib_etl_forge_ingest_common::prune_children;

pub use datalib_etl::doltlite_raw::db_path_for;

use super::schema_raw::{
    full_ddl, IssueCommentRow, PrReviewCommentRow, PrReviewRow, PullRequestRow, SelfIdentityRow,
};

datalib_etl::raw_db!(pub RawDb: EntityStore, full_ddl());

impl RawDb {
    // ── self_identity ───────────────────────────────────────────────

    pub async fn upsert_self_identity(&self, payload: &Value) -> Result<()> {
        bulk_upsert(self.pool(), &[SelfIdentityRow::from_payload(payload)?]).await
    }

    pub async fn load_self_identity(&self) -> Result<Option<Value>> {
        // Audited: the only interpolation is a table name this handle
        // chose -- a literal, or that literal behind `pinned_`.
        let row = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT json(payload) AS payload FROM {} \
             WHERE payload IS NOT NULL ORDER BY id LIMIT 1",
            self.reads().table("self_identity")
        )))
        .fetch_optional(self.pool())
        .await
        .context("select self_identity")?;
        let Some(row) = row else { return Ok(None) };
        let payload: Option<String> = row.try_get("payload").ok();
        Ok(payload.and_then(|s| serde_json::from_str(&s).ok()))
    }

    // ── pull_requests ───────────────────────────────────────────────

    pub async fn upsert_pull_request(&self, repo: &str, num: u32, payload: &Value) -> Result<()> {
        bulk_upsert(
            self.pool(),
            &[PullRequestRow::from_payload(repo, num, payload)?],
        )
        .await
    }

    // ── issue_comments / pr_reviews / pr_review_comments ────────────

    /// One PR's whole list from one child `table`, in one transaction.
    pub async fn upsert_children(
        &self,
        table: &str,
        repo: &str,
        num: u32,
        payloads: &[Value],
    ) -> Result<()> {
        fn rows<T>(payloads: &[Value], row: impl Fn(&Value) -> Result<T>) -> Result<Vec<T>> {
            payloads.iter().map(row).collect()
        }
        let pool = self.pool();
        match table {
            "issue_comments" => {
                let rows = rows(payloads, |p| IssueCommentRow::from_payload(repo, num, p))?;
                bulk_upsert(pool, &rows).await
            }
            "pr_reviews" => {
                let rows = rows(payloads, |p| PrReviewRow::from_payload(repo, num, p))?;
                bulk_upsert(pool, &rows).await
            }
            "pr_review_comments" => {
                let rows = rows(payloads, |p| PrReviewCommentRow::from_payload(repo, num, p))?;
                bulk_upsert(pool, &rows).await
            }
            other => anyhow::bail!("{other} is not a PR child table"),
        }
    }

    /// Drop this PR's rows in `table` that the fresh listing did not name.
    pub async fn prune_pr_children(
        &self,
        table: &'static str,
        repo: &str,
        num: u32,
        keep: &HashSet<String>,
    ) -> Result<usize> {
        let num = num.to_string();
        prune_children(
            self.pool(),
            table,
            &[("repo_full_name", repo), ("pr_number", &num)],
            keep,
        )
        .await
    }

    pub async fn load_pull_requests(&self) -> Result<Vec<LoadedPullRequest>> {
        // Audited: as `load_self_identity`.
        let rows = sqlx::query(sqlx::AssertSqlSafe(format!(
            "SELECT id, repo_full_name, pr_number, json(payload) AS payload
             FROM {} WHERE payload IS NOT NULL ORDER BY id",
            self.reads().table("pull_requests")
        )))
        .fetch_all(self.pool())
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
        // Audited: `table` is a static identifier supplied by us, not user
        // input; `payload` is read, nothing is interpolated from it.
        let sql = format!(
            "SELECT id, repo_full_name, pr_number, json(payload) AS payload
             FROM {} WHERE payload IS NOT NULL ORDER BY id",
            self.reads().table(table)
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_all(self.pool())
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

    pub async fn any_pull_requests(&self) -> Result<bool> {
        let row = sqlx::query("SELECT 1 FROM pull_requests WHERE payload IS NOT NULL LIMIT 1")
            .fetch_optional(self.pool())
            .await
            .context("any_pull_requests")?;
        Ok(row.is_some())
    }
}

/// One PR row loaded back out of the DB. Carries the upstream-stable
/// composite id (`<repo>#<num>`), the promoted scoping columns, and the
/// raw payload that the render layer parses.
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

/// Bag returned to the synchronous render path. GitHub doesn't ship
/// any binary blobs, so there's no blob handle here.
#[derive(Clone, Default)]
pub struct LoadedRaw {
    pub self_identity: Option<Value>,
    pub pull_requests: Vec<LoadedPullRequest>,
    pub issue_comments: Vec<LoadedChild>,
    pub pr_reviews: Vec<LoadedChild>,
    pub pr_review_comments: Vec<LoadedChild>,
}

/// Synchronous helper for non-async callers (render, synthesize) that
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
        db.upsert_children(
            "issue_comments",
            "octocat/hello",
            7,
            &[json!({"id": 101, "body": "hi", "user": {"login": "alice"}})],
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
        datalib_etl::doltlite_raw::upsert_scope_state(
            db.pool(),
            "author:@me",
            "2026-05-21T02:00:00+02:00",
        )
        .await
        .unwrap();
        let m = datalib_etl::doltlite_raw::load_scope_state(db.pool())
            .await
            .unwrap();
        // Stored as UTC; `since_for_scope` is what turns it back into an
        // API-shaped value.
        assert_eq!(
            m.get("author:@me").map(String::as_str),
            Some("2026-05-21T00:00:00.000000+00:00")
        );
    }
}
