//! Raw-store schema for the GitHub provider.

use datalib_etl::doltlite_raw::{self as dr, WirePayload, WirePayloadRow};
use datalib_etl_forge_ingest_common::{numeric_id, opt_str};
use datalib_etl_macros::WirePayloadRow;
use serde_json::Value;

/// Names of the entity tables, in the order they should be iterated
/// for full-table operations (truncate, full-DDL composition, etc.).
pub const DATA_TABLES: &[&str] = &[
    "self_identity",
    "pull_requests",
    "issue_comments",
    "pr_reviews",
    "pr_review_comments",
];

/// `self_identity` — exactly one row holding the authenticated user's
/// `GET /user` response.
///
/// PK choice: upstream GitHub user id (numeric, stringified). One row
/// per authenticated account.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "self_identity")]
pub struct SelfIdentityRow {
    pub id_and_payload: WirePayload,
    pub login: Option<String>,
    pub html_url: Option<String>,
}

impl SelfIdentityRow {
    pub fn from_payload(payload: &Value) -> anyhow::Result<Self> {
        let id = numeric_id(payload, "/user response")?;
        Ok(Self {
            id_and_payload: WirePayload {
                id,
                payload: serde_json::to_string(payload)?,
            },
            login: opt_str(payload, "login"),
            html_url: opt_str(payload, "html_url"),
        })
    }
}

/// `pull_requests` — one row per PR we have ever fetched.
///
/// PK choice: composite `"<repo_full_name>#<pr_number>"`, synthesized
/// by [`pr_pk`]. GitHub's per-PR numeric id is repo-scoped rather than
/// global, so we hand-roll a composite that's both upstream-stable and
/// known straight from a search hit (no detail-fetch needed to learn
/// the PK).
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "pull_requests")]
pub struct PullRequestRow {
    pub id_and_payload: WirePayload,
    pub repo_full_name: String,
    pub pr_number: i64,
    pub state: Option<String>,
    pub html_url: Option<String>,
    pub head_sha: Option<String>,
    pub base_sha: Option<String>,
    pub head_ref: Option<String>,
    pub base_ref: Option<String>,
    pub updated_at: Option<String>,
    pub merged_at: Option<String>,
}

impl PullRequestRow {
    pub fn from_payload(repo: &str, num: u32, payload: &Value) -> anyhow::Result<Self> {
        let head = payload.get("head");
        let base = payload.get("base");
        Ok(Self {
            id_and_payload: WirePayload {
                id: pr_pk(repo, num),
                payload: serde_json::to_string(payload)?,
            },
            repo_full_name: repo.to_string(),
            pr_number: num as i64,
            state: opt_str(payload, "state"),
            html_url: opt_str(payload, "html_url"),
            head_sha: head.and_then(|h| opt_str(h, "sha")),
            base_sha: base.and_then(|b| opt_str(b, "sha")),
            head_ref: head.and_then(|h| opt_str(h, "ref")),
            base_ref: base.and_then(|b| opt_str(b, "ref")),
            updated_at: opt_str(payload, "updated_at"),
            merged_at: opt_str(payload, "merged_at"),
        })
    }
}

/// Index on `pull_requests(repo_full_name, pr_number)` — supports the
/// "all PRs for this repo" filter that render / synthesize use, and
/// the per-PR child joins.
pub const PULL_REQUESTS_BY_REPO_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS pull_requests_by_repo ON pull_requests(repo_full_name, pr_number)";

/// `issue_comments` — one row per "conversation" comment on a PR's
/// underlying issue.
///
/// PK choice: stringified GitHub-global numeric `id`. GitHub's issue
/// comment id space is global so no `<repo>#` prefix is needed.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "issue_comments")]
pub struct IssueCommentRow {
    pub id_and_payload: WirePayload,
    pub repo_full_name: String,
    pub pr_number: i64,
    pub html_url: Option<String>,
    pub user_login: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

impl IssueCommentRow {
    pub fn from_payload(repo: &str, num: u32, payload: &Value) -> anyhow::Result<Self> {
        let id = numeric_id(payload, "issue_comment")?;
        Ok(Self {
            id_and_payload: WirePayload {
                id,
                payload: serde_json::to_string(payload)?,
            },
            repo_full_name: repo.to_string(),
            pr_number: num as i64,
            html_url: opt_str(payload, "html_url"),
            user_login: payload.get("user").and_then(|u| opt_str(u, "login")),
            created_at: opt_str(payload, "created_at"),
            updated_at: opt_str(payload, "updated_at"),
        })
    }
}

/// Index on `issue_comments(repo_full_name, pr_number)` — supports the
/// per-PR child join that render uses to assemble one document per
/// PR.
pub const ISSUE_COMMENTS_BY_PR_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS issue_comments_by_pr ON issue_comments(repo_full_name, pr_number)";

/// `pr_reviews` — one row per PR review (the wrapping
/// approve / request-changes / comment event, not the individual
/// inline comments).
///
/// PK choice: stringified GitHub-global numeric `id`. Review id space
/// is disjoint from the comment id spaces.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "pr_reviews")]
pub struct PrReviewRow {
    pub id_and_payload: WirePayload,
    pub repo_full_name: String,
    pub pr_number: i64,
    pub state: Option<String>,
    pub commit_id: Option<String>,
    pub user_login: Option<String>,
    pub submitted_at: Option<String>,
    pub html_url: Option<String>,
}

impl PrReviewRow {
    pub fn from_payload(repo: &str, num: u32, payload: &Value) -> anyhow::Result<Self> {
        let id = numeric_id(payload, "pr_review")?;
        Ok(Self {
            id_and_payload: WirePayload {
                id,
                payload: serde_json::to_string(payload)?,
            },
            repo_full_name: repo.to_string(),
            pr_number: num as i64,
            state: opt_str(payload, "state"),
            commit_id: opt_str(payload, "commit_id"),
            user_login: payload.get("user").and_then(|u| opt_str(u, "login")),
            submitted_at: opt_str(payload, "submitted_at"),
            html_url: opt_str(payload, "html_url"),
        })
    }
}

/// Index on `pr_reviews(repo_full_name, pr_number)` — supports the
/// per-PR child join.
pub const PR_REVIEWS_BY_PR_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS pr_reviews_by_pr ON pr_reviews(repo_full_name, pr_number)";

/// `pr_review_comments` — one row per inline / diff-anchored review
/// comment.
///
/// PK choice: stringified GitHub-global numeric `id`. Review-comment
/// id space is disjoint from issue-comment and review id spaces.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "pr_review_comments")]
pub struct PrReviewCommentRow {
    pub id_and_payload: WirePayload,
    pub repo_full_name: String,
    pub pr_number: i64,
    pub in_reply_to_id: Option<i64>,
    pub pull_request_review_id: Option<i64>,
    pub html_url: Option<String>,
    pub user_login: Option<String>,
    pub path: Option<String>,
    pub line: Option<i64>,
    pub original_line: Option<i64>,
    pub commit_id: Option<String>,
    pub original_commit_id: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
}

impl PrReviewCommentRow {
    pub fn from_payload(repo: &str, num: u32, payload: &Value) -> anyhow::Result<Self> {
        let id = numeric_id(payload, "pr_review_comment")?;
        Ok(Self {
            id_and_payload: WirePayload {
                id,
                payload: serde_json::to_string(payload)?,
            },
            repo_full_name: repo.to_string(),
            pr_number: num as i64,
            in_reply_to_id: payload.get("in_reply_to_id").and_then(|v| v.as_i64()),
            pull_request_review_id: payload
                .get("pull_request_review_id")
                .and_then(|v| v.as_i64()),
            html_url: opt_str(payload, "html_url"),
            user_login: payload.get("user").and_then(|u| opt_str(u, "login")),
            path: opt_str(payload, "path"),
            line: payload.get("line").and_then(|v| v.as_i64()),
            original_line: payload.get("original_line").and_then(|v| v.as_i64()),
            commit_id: opt_str(payload, "commit_id"),
            original_commit_id: opt_str(payload, "original_commit_id"),
            created_at: opt_str(payload, "created_at"),
            updated_at: opt_str(payload, "updated_at"),
        })
    }
}

/// Index on `pr_review_comments(repo_full_name, pr_number)` — supports
/// the per-PR child join.
pub const PR_REVIEW_COMMENTS_BY_PR_INDEX_DDL: &str = "CREATE INDEX IF NOT EXISTS \
     pr_review_comments_by_pr ON pr_review_comments(repo_full_name, pr_number)";

/// Recipe for the synthesized [`PULL_REQUESTS_DDL`] primary key.
pub fn pr_pk(repo: &str, num: u32) -> String {
    format!("{repo}#{num}")
}

/// Compose the full DDL list passed to
/// [`datalib_etl::doltlite_raw::open`]: every entity table DDL,
/// each entity's CREATE-INDEX statements, and the paired
/// `<table>_bookkeeping` DDL produced by the shared layer.
pub fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = vec![
        SelfIdentityRow::ddl(),
        PullRequestRow::ddl(),
        PULL_REQUESTS_BY_REPO_INDEX_DDL.to_string(),
        IssueCommentRow::ddl(),
        ISSUE_COMMENTS_BY_PR_INDEX_DDL.to_string(),
        PrReviewRow::ddl(),
        PR_REVIEWS_BY_PR_INDEX_DDL.to_string(),
        PrReviewCommentRow::ddl(),
        PR_REVIEW_COMMENTS_BY_PR_INDEX_DDL.to_string(),
    ];
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}
