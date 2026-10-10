//! Doltlite-backed raw store for the GitHub provider.

use std::collections::HashSet;
use std::path::Path;

use anyhow::{Context, Result};
use datalib_time::IsoOffsetTimestamp;
use serde_json::Value;
use sqlx::{Row, Sqlite, Transaction};

use datalib_etl::bulk::{bulk_upsert, bulk_upsert_entity_in_tx, bulk_upsert_in_tx};
use datalib_etl_forge_ingest_common::{load_self_identity, prune_children, row_payload};

pub use datalib_etl::doltlite_raw::db_path_for;

use super::schema_raw::{
    full_ddl, IssueCommentRow, PrReviewCommentRow, PrReviewRow, PullRequestRow, SelfIdentityRow,
    LADDER,
};

datalib_etl::raw_db!(pub RawDb: EntityStore, full_ddl(), LADDER);

impl RawDb {
    // ── self_identity ───────────────────────────────────────────────

    pub async fn upsert_self_identity(&self, payload: &Value) -> Result<()> {
        bulk_upsert(self.pool(), &[SelfIdentityRow::from_payload(payload)?]).await
    }

    pub async fn load_self_identity(&self) -> Result<Option<Value>> {
        load_self_identity(self.pool()).await
    }

    // ── pull_requests ───────────────────────────────────────────────

    /// The record alone: its sidecar is written by the fetch loop, in
    /// the same transaction, with the version it is held at.
    pub async fn store_pull_request(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        repo: &str,
        num: u32,
        payload: &Value,
    ) -> Result<()> {
        bulk_upsert_entity_in_tx(tx, &[PullRequestRow::from_payload(repo, num, payload)?]).await
    }

    // ── issue_comments / pr_reviews / pr_review_comments ────────────

    /// One PR's whole list from one child `table`.
    pub async fn store_children(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        table: &str,
        repo: &str,
        num: u32,
        payloads: &[Value],
        now: &IsoOffsetTimestamp,
    ) -> Result<()> {
        fn rows<T>(payloads: &[Value], row: impl Fn(&Value) -> Result<T>) -> Result<Vec<T>> {
            payloads.iter().map(row).collect()
        }
        match table {
            "issue_comments" => {
                let rows = rows(payloads, |p| IssueCommentRow::from_payload(repo, num, p))?;
                bulk_upsert_in_tx(tx, &rows, now).await
            }
            "pr_reviews" => {
                let rows = rows(payloads, |p| PrReviewRow::from_payload(repo, num, p))?;
                bulk_upsert_in_tx(tx, &rows, now).await
            }
            "pr_review_comments" => {
                let rows = rows(payloads, |p| PrReviewCommentRow::from_payload(repo, num, p))?;
                bulk_upsert_in_tx(tx, &rows, now).await
            }
            other => anyhow::bail!("{other} is not a PR child table"),
        }
    }

    /// Drop this PR's rows in `table` that the fresh listing did not name.
    pub async fn prune_pr_children(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        table: &'static str,
        repo: &str,
        num: u32,
        keep: &HashSet<String>,
    ) -> Result<usize> {
        let num = num.to_string();
        prune_children(
            tx,
            table,
            &[("repo_full_name", repo), ("pr_number", &num)],
            keep,
        )
        .await
    }

    pub async fn load_pull_requests(&self) -> Result<Vec<LoadedPullRequest>> {
        let rows = self.load_children("pull_requests").await?;
        Ok(rows
            .into_iter()
            .map(|c| LoadedPullRequest {
                id: c.id,
                repo_full_name: c.repo_full_name,
                pr_number: c.pr_number,
                payload: c.payload,
            })
            .collect())
    }

    pub async fn load_children(&self, table: &str) -> Result<Vec<LoadedChild>> {
        // Audited: `table` is a static identifier supplied by us, not user
        // input; `payload` is read, nothing is interpolated from it.
        let sql = format!(
            "SELECT id, repo_full_name, pr_number, json(payload) AS payload
             FROM {} WHERE payload IS NOT NULL ORDER BY id",
            table
        );
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_all(self.pool())
            .await
            .with_context(|| format!("select {table}"))?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let Some(payload) = row_payload(&r)? else {
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
        let mut tx = db.pool().begin().await.unwrap();
        db.store_pull_request(
            &mut tx,
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
        db.store_children(
            &mut tx,
            "issue_comments",
            "octocat/hello",
            7,
            &[json!({"id": 101, "body": "hi", "user": {"login": "alice"}})],
            &IsoOffsetTimestamp::now_local(),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
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
        let mut tx = db.pool().begin().await.unwrap();
        db.store_pull_request(
            &mut tx,
            "octocat/hello",
            7,
            &json!({"number": 7, "title": "T"}),
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let row = sqlx::query(
            "SELECT typeof(payload) AS t FROM pull_requests WHERE id='octocat/hello#7'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        let t: String = row.try_get("t").unwrap();
        assert_eq!(t, "blob", "payload should be JSONB-encoded BLOB");
    }
}
