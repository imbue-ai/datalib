//! Parse the GitHub doltlite database written by [`crate::download`] into
//! in-memory rows for the renderer + grid_rows pass. Each PR's
//! `issue_comments`, `pr_reviews`, and `pr_review_comments` collapse
//! into one `CommentRow` stream sorted (per render) by section, then by
//! file/line, then chronologically within a thread.

use std::path::Path;

use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use serde_json::Value;
use uuid::Uuid;

use crate::download::db::{block_on_load_all, db_path_for, LoadedChild, LoadedRaw};

pub const ENTITY_SELF: &str = "self_identity";
pub const ENTITY_PR: &str = "pull_request";
pub const ENTITY_ISSUE_COMMENT: &str = "issue_comment";
pub const ENTITY_PR_REVIEW: &str = "pr_review";
pub const ENTITY_PR_REVIEW_COMMENT: &str = "pr_review_comment";

pub static GITHUB_UUID_NS: Lazy<Uuid> = Lazy::new(|| {
    Uuid::parse_str("b1a90c3a-1f7f-5d4b-9a23-7e3f2b8d0001").expect("valid github ns uuid")
});

pub fn github_pr_uuid(repo: &str, number: u32) -> String {
    Uuid::new_v5(
        &GITHUB_UUID_NS,
        format!("github:{repo}:pr:{number}").as_bytes(),
    )
    .to_string()
}
pub fn github_issue_comment_uuid(repo: &str, id: i64) -> String {
    Uuid::new_v5(
        &GITHUB_UUID_NS,
        format!("github:{repo}:issue_comment:{id}").as_bytes(),
    )
    .to_string()
}
pub fn github_review_uuid(repo: &str, id: i64) -> String {
    Uuid::new_v5(
        &GITHUB_UUID_NS,
        format!("github:{repo}:pr_review:{id}").as_bytes(),
    )
    .to_string()
}
pub fn github_review_comment_uuid(repo: &str, id: i64) -> String {
    Uuid::new_v5(
        &GITHUB_UUID_NS,
        format!("github:{repo}:pr_review_comment:{id}").as_bytes(),
    )
    .to_string()
}

#[derive(Debug, Clone, Default)]
pub struct GithubSelfIdentity {
    pub user_id: Option<i64>,
    pub login: Option<String>,
    pub html_url: Option<String>,
    pub raw: Value,
}

#[derive(Debug, Clone)]
pub struct PullRequestRow {
    pub uuid: String,
    pub repo_full_name: String,
    pub pr_number: u32,
    pub title: String,
    pub body: String,
    pub state: Option<String>,
    pub html_url: Option<String>,
    pub head_sha: Option<String>,
    pub base_sha: Option<String>,
    pub head_ref: Option<String>,
    pub base_ref: Option<String>,
    pub user_login: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub merged_at: Option<String>,
}

/// Which logical bucket a comment falls into for the per-PR render.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommentSection {
    /// `pr_review` row — body + state on the review itself.
    Review,
    /// `issue_comment` — general PR conversation tab.
    General,
    /// `pr_review_comment` — line-anchored diff comment (path + line).
    Inline,
}

impl CommentSection {
    pub fn entity(self) -> &'static str {
        match self {
            CommentSection::Review => ENTITY_PR_REVIEW,
            CommentSection::General => ENTITY_ISSUE_COMMENT,
            CommentSection::Inline => ENTITY_PR_REVIEW_COMMENT,
        }
    }
}

#[derive(Debug, Clone)]
pub struct CommentRow {
    pub uuid: String,
    pub repo_full_name: String,
    pub pr_number: u32,
    pub kind: &'static str,
    pub section: CommentSection,
    pub external_id: i64,
    /// Inline only: parent comment for replies. Top-level comments use `None`.
    pub in_reply_to_id: Option<i64>,
    pub user_login: Option<String>,
    pub body: String,
    pub html_url: Option<String>,
    pub path: Option<String>,
    pub line: Option<i64>,
    pub commit_id: Option<String>,
    pub created_at: String,
    pub updated_at: Option<String>,
    /// Review state (`APPROVED`, `CHANGES_REQUESTED`, `COMMENTED`, …).
    /// Only set on `Review` rows.
    pub state: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct ParsedGithubApi {
    pub self_identity: Option<GithubSelfIdentity>,
    /// The PRs to render this pass — narrowed to the buckets the diff
    /// named, or every PR on a cold start.
    pub pull_requests: Vec<PullRequestRow>,
    pub comments: Vec<CommentRow>,
    /// PRs the diff reported unchanged, so this run skipped them.
    pub docs_skipped: usize,
    pub scan: ScanResult,
    /// Buckets the diff named whose `pull_requests` row is gone: PRs that
    /// vanished from the store. Empty on a cold start, which looks at
    /// every bucket and so has nothing to compare against.
    pub vanished_buckets: Vec<String>,
}

/// Result of the `dolt_diff` scan, carried alongside the parsed bag so
/// render can advance the cursor and log the timing.
#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    /// `Some(set)` → render only these `"{repo}#{num}"` buckets. `None` →
    /// cold start (no cursor, or the diff was unusable): render everything.
    pub changed_buckets: Option<std::collections::HashSet<String>>,
    /// HEAD at scan time, to stamp into the render cursor on success.
    pub new_head: Option<String>,
    pub scan_elapsed: Option<std::time::Duration>,
}

/// Read raw payloads out of the doltlite DB. `path` may be either a
/// `.doltlite_db` file or the per-source directory (whose entity db is
/// `entities.doltlite_db`) — both resolve to the same sqlite file via
/// [`db_path_for`].
/// Parse for render, narrowed by `dolt_diff` when a render cursor says
/// where the last run got to.
///
/// `last_render_hash` is that cursor. `None` renders everything, which is
/// always correct and is what a first run does.
pub fn parse_api_dir(path: &Path, last_render_hash: Option<&str>) -> Result<ParsedGithubApi> {
    let db_path = db_path_for(path);
    if !db_path.exists() {
        // No store: this source has never been downloaded. That is
        // the normal state of every source in a freshly scaffolded
        // config, not an error — render nothing and succeed. A store
        // that exists but can't be read still fails, below. See
        // docs/dev/step_protocol.md, "Rendering a source with no data".
        return Ok(ParsedGithubApi::default());
    }
    let raw = block_on_load_all(&db_path)
        .with_context(|| format!("load github db {}", db_path.display()))?;
    let mut parsed = parse_loaded(raw);

    let scan = block_on_scan(&db_path, last_render_hash)?;
    if let Some(changed) = scan.changed_buckets.as_ref() {
        let before = parsed.pull_requests.len();
        parsed
            .pull_requests
            .retain(|pr| changed.contains(&pr_pk(&pr.repo_full_name, pr.pr_number)));
        parsed.docs_skipped = before.saturating_sub(parsed.pull_requests.len());
        // Comments follow their PR: one left attached to a PR this pass is
        // not rendering would be grouped into a document nobody emits.
        parsed
            .comments
            .retain(|c| changed.contains(&pr_pk(&c.repo_full_name, c.pr_number)));
        parsed.vanished_buckets = block_on_vanished(&db_path, changed)?;
    }
    parsed.scan = scan;
    Ok(parsed)
}

/// The bucket key a PR's rows share: `pull_requests.id`, and the same
/// string composed from any child row's `(repo_full_name, pr_number)`.
fn pr_pk(repo: &str, num: u32) -> String {
    format!("{repo}#{num}")
}

fn block_on_scan(db_path: &Path, last_render_hash: Option<&str>) -> Result<ScanResult> {
    let path = db_path.to_path_buf();
    let last = last_render_hash.map(str::to_string);
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let pool = open_ro(&path).await?;
            let scan = datalib_etl::doltlite_raw::scan_buckets(
                &pool,
                last.as_deref(),
                &datalib_etl::doltlite_raw::DiffScanSpec {
                    // `self_identity` is not read by render, so a change to
                    // it fans out to nothing.
                    global_fanout_tables: &[],
                    bucket_query: BUCKET_QUERY,
                },
            )
            .await?;
            pool.close().await;
            Ok::<_, anyhow::Error>(ScanResult {
                changed_buckets: scan.changed_buckets,
                new_head: scan.new_head,
                scan_elapsed: scan.scan_elapsed,
            })
        })
    })
}

fn block_on_vanished(
    db_path: &Path,
    changed: &std::collections::HashSet<String>,
) -> Result<Vec<String>> {
    let path = db_path.to_path_buf();
    let changed = changed.clone();
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let pool = open_ro(&path).await?;
            let gone = datalib_etl::doltlite_raw::buckets_without_rows(
                &pool,
                &changed,
                &[("pull_requests", "id")],
            )
            .await?;
            pool.close().await;
            Ok::<_, anyhow::Error>(gone)
        })
    })
}

async fn open_ro(db_path: &Path) -> Result<sqlx::SqlitePool> {
    use std::str::FromStr;
    let opts =
        sqlx::sqlite::SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))?
            .read_only(true);
    sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .with_context(|| format!("open github doltlite for render {}", db_path.display()))
}

/// A PR's document is built from its own row plus its three child tables,
/// so any of the four moving means that PR must re-render. Children carry
/// `(repo_full_name, pr_number)`, which composes the same key
/// `pull_requests.id` already holds.
const BUCKET_QUERY: &str = "
    SELECT DISTINCT bucket FROM (
        SELECT coalesce(to_id, from_id) AS bucket
          FROM dolt_diff_pull_requests
         WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
        UNION
        SELECT coalesce(to_repo_full_name, from_repo_full_name) || '#' ||
               coalesce(to_pr_number, from_pr_number)
          FROM dolt_diff_issue_comments
         WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
        UNION
        SELECT coalesce(to_repo_full_name, from_repo_full_name) || '#' ||
               coalesce(to_pr_number, from_pr_number)
          FROM dolt_diff_pr_reviews
         WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
        UNION
        SELECT coalesce(to_repo_full_name, from_repo_full_name) || '#' ||
               coalesce(to_pr_number, from_pr_number)
          FROM dolt_diff_pr_review_comments
         WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
    )
    WHERE bucket IS NOT NULL
";

pub fn parse_loaded(raw: LoadedRaw) -> ParsedGithubApi {
    let mut out = ParsedGithubApi::default();

    if let Some(s) = raw.self_identity {
        out.self_identity = Some(GithubSelfIdentity {
            user_id: s.get("id").and_then(|v| v.as_i64()),
            login: s.get("login").and_then(|v| v.as_str()).map(String::from),
            html_url: s.get("html_url").and_then(|v| v.as_str()).map(String::from),
            raw: s,
        });
    }

    for pr in raw.pull_requests {
        let repo = pr.repo_full_name;
        let num = pr.pr_number;
        if repo.is_empty() || num == 0 {
            continue;
        }
        let p = &pr.payload;
        out.pull_requests.push(PullRequestRow {
            uuid: github_pr_uuid(&repo, num),
            repo_full_name: repo,
            pr_number: num,
            title: p.get("title").and_then(|v| v.as_str()).unwrap_or("").into(),
            body: p.get("body").and_then(|v| v.as_str()).unwrap_or("").into(),
            state: p.get("state").and_then(|v| v.as_str()).map(String::from),
            html_url: p.get("html_url").and_then(|v| v.as_str()).map(String::from),
            head_sha: p
                .get("head")
                .and_then(|h| h.get("sha"))
                .and_then(|v| v.as_str())
                .map(String::from),
            base_sha: p
                .get("base")
                .and_then(|b| b.get("sha"))
                .and_then(|v| v.as_str())
                .map(String::from),
            head_ref: p
                .get("head")
                .and_then(|h| h.get("ref"))
                .and_then(|v| v.as_str())
                .map(String::from),
            base_ref: p
                .get("base")
                .and_then(|b| b.get("ref"))
                .and_then(|v| v.as_str())
                .map(String::from),
            user_login: p
                .get("user")
                .and_then(|u| u.get("login"))
                .and_then(|v| v.as_str())
                .map(String::from),
            created_at: p
                .get("created_at")
                .and_then(|v| v.as_str())
                .map(String::from),
            updated_at: p
                .get("updated_at")
                .and_then(|v| v.as_str())
                .map(String::from),
            merged_at: p
                .get("merged_at")
                .and_then(|v| v.as_str())
                .map(String::from),
        });
    }

    push_issue_comments(&mut out.comments, raw.issue_comments);
    push_reviews(&mut out.comments, raw.pr_reviews);
    push_review_comments(&mut out.comments, raw.pr_review_comments);

    out
}

fn push_issue_comments(out: &mut Vec<CommentRow>, rows: Vec<LoadedChild>) {
    for c in rows {
        let id = c.payload.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        if c.repo_full_name.is_empty() || c.pr_number == 0 || id == 0 {
            continue;
        }
        let p = &c.payload;
        out.push(CommentRow {
            uuid: github_issue_comment_uuid(&c.repo_full_name, id),
            repo_full_name: c.repo_full_name,
            pr_number: c.pr_number,
            kind: "GitHub PR Comment",
            section: CommentSection::General,
            external_id: id,
            in_reply_to_id: None,
            user_login: p
                .get("user")
                .and_then(|u| u.get("login"))
                .and_then(|v| v.as_str())
                .map(String::from),
            body: p.get("body").and_then(|v| v.as_str()).unwrap_or("").into(),
            html_url: p.get("html_url").and_then(|v| v.as_str()).map(String::from),
            path: None,
            line: None,
            commit_id: None,
            created_at: p
                .get("created_at")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .into(),
            updated_at: p
                .get("updated_at")
                .and_then(|v| v.as_str())
                .map(String::from),
            state: None,
        });
    }
}

fn push_reviews(out: &mut Vec<CommentRow>, rows: Vec<LoadedChild>) {
    for r in rows {
        let id = r.payload.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        if r.repo_full_name.is_empty() || r.pr_number == 0 || id == 0 {
            continue;
        }
        let p = &r.payload;
        out.push(CommentRow {
            uuid: github_review_uuid(&r.repo_full_name, id),
            repo_full_name: r.repo_full_name,
            pr_number: r.pr_number,
            kind: "GitHub Review",
            section: CommentSection::Review,
            external_id: id,
            in_reply_to_id: None,
            user_login: p
                .get("user")
                .and_then(|u| u.get("login"))
                .and_then(|v| v.as_str())
                .map(String::from),
            body: p.get("body").and_then(|v| v.as_str()).unwrap_or("").into(),
            html_url: p.get("html_url").and_then(|v| v.as_str()).map(String::from),
            path: None,
            line: None,
            commit_id: p
                .get("commit_id")
                .and_then(|v| v.as_str())
                .map(String::from),
            created_at: p
                .get("submitted_at")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .into(),
            updated_at: None,
            state: p.get("state").and_then(|v| v.as_str()).map(String::from),
        });
    }
}

fn push_review_comments(out: &mut Vec<CommentRow>, rows: Vec<LoadedChild>) {
    for c in rows {
        let id = c.payload.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        if c.repo_full_name.is_empty() || c.pr_number == 0 || id == 0 {
            continue;
        }
        let p = &c.payload;
        let in_reply_to = p.get("in_reply_to_id").and_then(|v| v.as_i64());
        let line = p
            .get("line")
            .and_then(|v| v.as_i64())
            .or_else(|| p.get("original_line").and_then(|v| v.as_i64()));
        let commit_id = p
            .get("commit_id")
            .and_then(|v| v.as_str())
            .or_else(|| p.get("original_commit_id").and_then(|v| v.as_str()))
            .map(String::from);
        out.push(CommentRow {
            uuid: github_review_comment_uuid(&c.repo_full_name, id),
            repo_full_name: c.repo_full_name,
            pr_number: c.pr_number,
            kind: "GitHub Review Comment",
            section: CommentSection::Inline,
            external_id: id,
            in_reply_to_id: in_reply_to,
            user_login: p
                .get("user")
                .and_then(|u| u.get("login"))
                .and_then(|v| v.as_str())
                .map(String::from),
            body: p.get("body").and_then(|v| v.as_str()).unwrap_or("").into(),
            html_url: p.get("html_url").and_then(|v| v.as_str()).map(String::from),
            path: p.get("path").and_then(|v| v.as_str()).map(String::from),
            line,
            commit_id,
            created_at: p
                .get("created_at")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .into(),
            updated_at: p
                .get("updated_at")
                .and_then(|v| v.as_str())
                .map(String::from),
            state: None,
        });
    }
}

#[cfg(test)]
mod no_data_tests {
    use super::*;

    /// A source that has never been downloaded renders as empty, not
    /// as a failure: that is the normal state of every source in a
    /// freshly scaffolded config. See docs/dev/step_protocol.md,
    /// "Rendering a source with no data".
    #[test]
    fn parse_missing_source_returns_empty_silently() {
        let parsed = parse_api_dir(Path::new("/this/does/not/exist"), None).unwrap();
        assert!(parsed.pull_requests.is_empty());
        assert!(parsed.comments.is_empty());
        assert!(parsed.self_identity.is_none());
    }
}
