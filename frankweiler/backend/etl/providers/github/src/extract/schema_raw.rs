//! Raw-store schema for the GitHub provider.
//!
//! Declarations-only, proto-flavored. See
//! [`docs/dev/data_architecture_ingestion.md`](../../../../../docs/dev/data_architecture_ingestion.md)
//! and [`docs/dev/data_architecture_plan.md`](../../../../../docs/dev/data_architecture_plan.md)
//! §P0.1 for the conventions every `schema_raw.rs` follows.
//!
//! GitHub-specific notes:
//!
//! - **Composite-id PR rows.** GitHub's per-PR numeric id is repo-scoped,
//!   not global, so `pull_requests.id` is the upstream-stable composite
//!   `"<repo_full_name>#<pr_number>"`; see [`pr_pk`]. The composite is
//!   known the moment discovery surfaces a search hit (the search item
//!   carries `repository_url` + `number`), so we can write the PR row
//!   without first cracking the detail payload.
//! - **Translate-side UUIDs are a separate recipe.** Cross-provider grid
//!   UUIDs (`github:{repo}:pr:{number}`, `github:{repo}:issue_comment:{id}`,
//!   …) are UUIDv5-derived in `crate::translate::parse`
//!   (`github_pr_uuid`, `github_issue_comment_uuid`,
//!   `github_review_uuid`, `github_review_comment_uuid`) — see
//!   `docs/dev/data_architecture_ingestion.md` §"Object identity". Those are
//!   translate concerns and don't appear in the raw store; the raw PKs
//!   here use the upstream-id strings directly.
//! - **Child-row PKs are globally-unique upstream ids.** `issue_comments`,
//!   `pr_reviews`, and `pr_review_comments` all use the stringified
//!   GitHub-global numeric id as their PK. Those id spaces are disjoint
//!   per endpoint so no namespacing prefix is needed.
//! - **Event-shaped children, summary-shaped parents.** PRs and their
//!   children carry `created_at` / `updated_at`. Translate sources
//!   `GridRow.when_ts` from `updated_at` on `pull_requests` and from
//!   `created_at` on the comment / review children.
//! - **Refresh-window cursor strategy.** Extract does not stream every
//!   PR every run; instead it searches `is:pr <scope> updated:>=<since>`,
//!   where `<since>` is either the persisted per-scope `last_seen_at`
//!   (sourced from the shared `sync_scope_state` bookkeeping table) or a
//!   cold-start floor of `today - refresh_window_days`. See
//!   `extract::since_for_scope`. The cursor itself lives in
//!   `sync_scope_state`, not in a dedicated table here.
//! - **`git_sha` + `external_id` cross-references.** `pull_requests`
//!   promotes `head_sha` / `base_sha` so cross-provider joins (e.g.
//!   GitLab MRs sharing a head commit, or local git history) can resolve
//!   via SHA without cracking the payload. `pr_review_comments`
//!   similarly promotes `commit_id` / `original_commit_id`.
//! - **Code-review-thread family with GitLab.** GitHub's
//!   `pr_review_comments` is the sibling of GitLab's MR diff-discussion
//!   notes; both promote `path` + `line` so a future cross-provider
//!   "show me every review comment on file F" index can be built without
//!   re-parsing payloads.

use frankweiler_etl::doltlite_raw::{self as dr, WirePayload, WirePayloadRow};
use frankweiler_etl_macros::WirePayloadRow;
use serde_json::Value;

/// Names of the entity tables, in the order they should be iterated
/// for full-table operations (truncate, full-DDL composition, etc.).
///
/// Used by `extract::db::RawDb::reset` to wipe per-row state without
/// touching blobs or bookkeeping. Also drives [`full_ddl`] when it
/// asks the shared layer for paired `<table>_bookkeeping` DDLs.
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
/// Provenance: `GET https://api.github.com/user`. Single-row identity
/// capture; the translate side reads it to label the `_source` block on
/// every grid row.
///
/// PK choice: upstream GitHub user id (numeric, stringified). One row
/// per authenticated account.
///
/// Columns:
/// - `id` — upstream `id` field, stringified. Primary key.
/// - `login` — denormalized `payload.login` for quick lookups.
/// - `html_url` — denormalized `payload.html_url` for quick lookups.
/// - `payload` — raw `/user` response (JSONB-encoded on disk).
///
/// Not event-shaped; no `when_ts` story.
#[derive(Debug, Clone, WirePayloadRow)]
#[wire_payload_row(table = "self_identity")]
pub struct SelfIdentityRow {
    pub id_and_payload: WirePayload,
    pub login: Option<String>,
    pub html_url: Option<String>,
}

impl SelfIdentityRow {
    /// Build from the upstream `GET /user` payload. Errors when the
    /// payload doesn't carry an `id` (i.e. we got back something we
    /// didn't recognize as a user object).
    pub fn from_payload(payload: &Value) -> anyhow::Result<Self> {
        let id = payload
            .get("id")
            .and_then(|v| v.as_i64())
            .map(|n| n.to_string())
            .ok_or_else(|| anyhow::anyhow!("/user response missing id"))?;
        Ok(Self {
            id_and_payload: WirePayload {
                id,
                payload: serde_json::to_string(payload)?,
            },
            login: payload
                .get("login")
                .and_then(|v| v.as_str())
                .map(String::from),
            html_url: payload
                .get("html_url")
                .and_then(|v| v.as_str())
                .map(String::from),
        })
    }
}

/// `pull_requests` — one row per PR we have ever fetched.
///
/// Provenance: `GET /repos/{owner}/{repo}/pulls/{num}` for each PR
/// surfaced by discovery (`/search/issues?q=is:pr <scope>`).
///
/// PK choice: composite `"<repo_full_name>#<pr_number>"`, synthesized
/// by [`pr_pk`]. GitHub's per-PR numeric id is repo-scoped rather than
/// global, so we hand-roll a composite that's both upstream-stable and
/// known straight from a search hit (no detail-fetch needed to learn
/// the PK).
///
/// Columns:
/// - `id` — synthesized composite PK (see [`pr_pk`]). Primary key.
/// - `repo_full_name` — owning repo, `"owner/name"`. Promoted scoping
///   column for `WHERE repo = ?` filters without cracking the payload.
/// - `pr_number` — PR number within the repo. Promoted scoping column.
/// - `state` — denormalized `payload.state` (`"open"` / `"closed"`).
/// - `html_url` — denormalized `payload.html_url`.
/// - `head_sha` / `base_sha` — promoted `payload.head.sha` /
///   `payload.base.sha`. Lets cross-provider joins (e.g. GitLab MRs
///   sharing a head commit, or local git history) resolve via SHA
///   without cracking the payload.
/// - `head_ref` / `base_ref` — promoted `payload.head.ref` /
///   `payload.base.ref`, branch names.
/// - `updated_at` — upstream `payload.updated_at` ISO-8601 stamp.
///   Sourced into `GridRow.when_ts` by translate.
/// - `merged_at` — upstream `payload.merged_at` ISO-8601 stamp, NULL
///   for unmerged PRs.
/// - `payload` — raw PR-detail JSON (JSONB-encoded on disk).
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
    /// Build from a `GET /repos/{owner}/{repo}/pulls/{num}` payload.
    /// The composite PK `"{repo}#{num}"` is known from the search hit
    /// — we don't have to crack the payload to learn it.
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
            state: payload
                .get("state")
                .and_then(|v| v.as_str())
                .map(String::from),
            html_url: payload
                .get("html_url")
                .and_then(|v| v.as_str())
                .map(String::from),
            head_sha: head
                .and_then(|h| h.get("sha"))
                .and_then(|v| v.as_str())
                .map(String::from),
            base_sha: base
                .and_then(|b| b.get("sha"))
                .and_then(|v| v.as_str())
                .map(String::from),
            head_ref: head
                .and_then(|h| h.get("ref"))
                .and_then(|v| v.as_str())
                .map(String::from),
            base_ref: base
                .and_then(|b| b.get("ref"))
                .and_then(|v| v.as_str())
                .map(String::from),
            updated_at: payload
                .get("updated_at")
                .and_then(|v| v.as_str())
                .map(String::from),
            merged_at: payload
                .get("merged_at")
                .and_then(|v| v.as_str())
                .map(String::from),
        })
    }
}

/// Index on `pull_requests(repo_full_name, pr_number)` — supports the
/// "all PRs for this repo" filter that translate / synthesize use, and
/// the per-PR child joins.
pub const PULL_REQUESTS_BY_REPO_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS pull_requests_by_repo ON pull_requests(repo_full_name, pr_number)";

/// `issue_comments` — one row per "conversation" comment on a PR's
/// underlying issue.
///
/// Provenance: `GET /repos/{owner}/{repo}/issues/{num}/comments`.
/// These are the top-level PR-thread comments (not file-anchored).
///
/// PK choice: stringified GitHub-global numeric `id`. GitHub's issue
/// comment id space is global so no `<repo>#` prefix is needed.
///
/// Columns:
/// - `id` — upstream `payload.id`, stringified. Primary key.
/// - `repo_full_name` — promoted scoping column.
/// - `pr_number` — promoted scoping column (the issue/PR this comment
///   lives on).
/// - `html_url` — denormalized `payload.html_url`.
/// - `user_login` — promoted `payload.user.login`.
/// - `created_at` — upstream `payload.created_at` ISO-8601 stamp.
///   Sourced into `GridRow.when_ts` by translate.
/// - `updated_at` — upstream `payload.updated_at` ISO-8601 stamp.
/// - `payload` — raw comment JSON (JSONB-encoded on disk).
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
        let id = payload
            .get("id")
            .and_then(|v| v.as_i64())
            .map(|n| n.to_string())
            .ok_or_else(|| anyhow::anyhow!("issue_comment missing id"))?;
        Ok(Self {
            id_and_payload: WirePayload {
                id,
                payload: serde_json::to_string(payload)?,
            },
            repo_full_name: repo.to_string(),
            pr_number: num as i64,
            html_url: payload
                .get("html_url")
                .and_then(|v| v.as_str())
                .map(String::from),
            user_login: payload
                .get("user")
                .and_then(|u| u.get("login"))
                .and_then(|v| v.as_str())
                .map(String::from),
            created_at: payload
                .get("created_at")
                .and_then(|v| v.as_str())
                .map(String::from),
            updated_at: payload
                .get("updated_at")
                .and_then(|v| v.as_str())
                .map(String::from),
        })
    }
}

/// Index on `issue_comments(repo_full_name, pr_number)` — supports the
/// per-PR child join that translate uses to assemble one document per
/// PR.
pub const ISSUE_COMMENTS_BY_PR_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS issue_comments_by_pr ON issue_comments(repo_full_name, pr_number)";

/// `pr_reviews` — one row per PR review (the wrapping
/// approve / request-changes / comment event, not the individual
/// inline comments).
///
/// Provenance: `GET /repos/{owner}/{repo}/pulls/{num}/reviews`.
///
/// PK choice: stringified GitHub-global numeric `id`. Review id space
/// is disjoint from the comment id spaces.
///
/// Columns:
/// - `id` — upstream `payload.id`, stringified. Primary key.
/// - `repo_full_name` — promoted scoping column.
/// - `pr_number` — promoted scoping column.
/// - `state` — denormalized `payload.state` (`"APPROVED"` /
///   `"CHANGES_REQUESTED"` / `"COMMENTED"` / …).
/// - `commit_id` — promoted `payload.commit_id`; the head SHA the
///   review was filed against. Cross-references `pull_requests.head_sha`
///   when the review is current.
/// - `user_login` — promoted `payload.user.login`.
/// - `submitted_at` — upstream `payload.submitted_at` ISO-8601 stamp.
///   Sourced into `GridRow.when_ts` by translate.
/// - `html_url` — denormalized `payload.html_url`.
/// - `payload` — raw review JSON (JSONB-encoded on disk).
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
        let id = payload
            .get("id")
            .and_then(|v| v.as_i64())
            .map(|n| n.to_string())
            .ok_or_else(|| anyhow::anyhow!("pr_review missing id"))?;
        Ok(Self {
            id_and_payload: WirePayload {
                id,
                payload: serde_json::to_string(payload)?,
            },
            repo_full_name: repo.to_string(),
            pr_number: num as i64,
            state: payload
                .get("state")
                .and_then(|v| v.as_str())
                .map(String::from),
            commit_id: payload
                .get("commit_id")
                .and_then(|v| v.as_str())
                .map(String::from),
            user_login: payload
                .get("user")
                .and_then(|u| u.get("login"))
                .and_then(|v| v.as_str())
                .map(String::from),
            submitted_at: payload
                .get("submitted_at")
                .and_then(|v| v.as_str())
                .map(String::from),
            html_url: payload
                .get("html_url")
                .and_then(|v| v.as_str())
                .map(String::from),
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
/// Provenance: `GET /repos/{owner}/{repo}/pulls/{num}/comments`. The
/// code-review-thread sibling of GitLab's MR diff-discussion notes.
///
/// PK choice: stringified GitHub-global numeric `id`. Review-comment
/// id space is disjoint from issue-comment and review id spaces.
///
/// Columns:
/// - `id` — upstream `payload.id`, stringified. Primary key.
/// - `repo_full_name` — promoted scoping column.
/// - `pr_number` — promoted scoping column.
/// - `in_reply_to_id` — promoted `payload.in_reply_to_id`; threads a
///   reply to its parent review comment.
/// - `pull_request_review_id` — promoted `payload.pull_request_review_id`;
///   joins the inline comment back to its wrapping `pr_reviews` row.
/// - `html_url` — denormalized `payload.html_url`.
/// - `user_login` — promoted `payload.user.login`.
/// - `path` — promoted `payload.path`; the file this comment is anchored
///   to. Lets a future cross-provider "every review comment on file F"
///   index avoid cracking the payload.
/// - `line` / `original_line` — promoted line anchors (current diff vs.
///   when the comment was first filed).
/// - `commit_id` / `original_commit_id` — promoted SHA anchors (current
///   commit the comment is positioned on vs. when first filed).
///   Cross-references `pull_requests.head_sha` and git history.
/// - `created_at` — upstream `payload.created_at` ISO-8601 stamp.
///   Sourced into `GridRow.when_ts` by translate.
/// - `updated_at` — upstream `payload.updated_at` ISO-8601 stamp.
/// - `payload` — raw review-comment JSON (JSONB-encoded on disk).
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
        let id = payload
            .get("id")
            .and_then(|v| v.as_i64())
            .map(|n| n.to_string())
            .ok_or_else(|| anyhow::anyhow!("pr_review_comment missing id"))?;
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
            html_url: payload
                .get("html_url")
                .and_then(|v| v.as_str())
                .map(String::from),
            user_login: payload
                .get("user")
                .and_then(|u| u.get("login"))
                .and_then(|v| v.as_str())
                .map(String::from),
            path: payload
                .get("path")
                .and_then(|v| v.as_str())
                .map(String::from),
            line: payload.get("line").and_then(|v| v.as_i64()),
            original_line: payload.get("original_line").and_then(|v| v.as_i64()),
            commit_id: payload
                .get("commit_id")
                .and_then(|v| v.as_str())
                .map(String::from),
            original_commit_id: payload
                .get("original_commit_id")
                .and_then(|v| v.as_str())
                .map(String::from),
            created_at: payload
                .get("created_at")
                .and_then(|v| v.as_str())
                .map(String::from),
            updated_at: payload
                .get("updated_at")
                .and_then(|v| v.as_str())
                .map(String::from),
        })
    }
}

/// Index on `pr_review_comments(repo_full_name, pr_number)` — supports
/// the per-PR child join.
pub const PR_REVIEW_COMMENTS_BY_PR_INDEX_DDL: &str = "CREATE INDEX IF NOT EXISTS \
     pr_review_comments_by_pr ON pr_review_comments(repo_full_name, pr_number)";

/// Recipe for the synthesized [`PULL_REQUESTS_DDL`] primary key.
///
/// GitHub's per-PR numeric id is repo-scoped (PR #7 in `octocat/hello`
/// is unrelated to PR #7 in `octocat/spoon-knife`), so we hand-roll a
/// composite from `(repo_full_name, pr_number)` — the only pair
/// guaranteed unique across the upstream universe. Format is
/// `"{repo}#{num}"`.
///
/// This is GitHub's analogue of the UUIDv5 recipes other providers
/// document under their (eventual, plan §P0.4) `uuid.rs` modules; the
/// translate-side cross-provider grid UUIDs
/// (`github_pr_uuid`, …) live in `crate::translate::parse` and key off
/// the same `(repo, number)` pair but feed a different namespace.
/// For now we keep the recipe **here** with the schema it keys into,
/// so that "what does the PK mean?" is one rustdoc-hop from the DDL.
/// When P0.4 lands we'll decide whether to relocate this recipe into
/// a sibling `uuid.rs` or leave it inline.
pub fn pr_pk(repo: &str, num: u32) -> String {
    format!("{repo}#{num}")
}

/// Compose the full DDL list passed to
/// [`frankweiler_etl::doltlite_raw::open`]: every entity table DDL,
/// each entity's CREATE-INDEX statements, and the paired
/// `<table>_bookkeeping` DDL produced by the shared layer.
///
/// Schema-local glue, kept here so the "what tables exist?" answer
/// is one function call from this file. Heavier composition (e.g. a
/// repo-wide bookkeeping macro) is deferred to P1.1.
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
