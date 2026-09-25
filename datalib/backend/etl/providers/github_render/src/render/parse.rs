//! Parse the GitHub doltlite database written by [`datalib_etl_github::ingest`] into
//! in-memory rows for the renderer + grid_rows pass. Each PR's
//! `issue_comments`, `pr_reviews`, and `pr_review_comments` collapse
//! into one `CommentRow` stream sorted (per render) by section, then by
//! file/line, then chronologically within a thread.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl_render::inputs::{changed_rows, RawRange};
use serde_json::Value;

use datalib_etl_github::ingest::db::{db_path_for, LoadedChild, LoadedRaw, RawDb};

pub use super::ids::{
    KIND_ISSUE_COMMENT as ENTITY_ISSUE_COMMENT, KIND_PR as ENTITY_PR,
    KIND_PR_REVIEW as ENTITY_PR_REVIEW, KIND_PR_REVIEW_COMMENT as ENTITY_PR_REVIEW_COMMENT,
};

pub const ENTITY_SELF: &str = "self_identity";

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
    /// `pull_requests.id`, the bucket key every row of this PR shares.
    pub row_id: String,
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
    /// The raw row this came from, for the PR's declaration.
    pub table: &'static str,
    pub row_id: String,
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
    /// The commit everything was read at.
    pub head: Option<String>,
    /// The buckets to render — `pull_requests.id`s; `None` renders
    /// everything.
    pub render: Option<HashSet<String>>,
}

/// Every table a PR's document reads; the forward scan diffs each.
const TABLES: [&str; 4] = [
    "pull_requests",
    "issue_comments",
    "pr_reviews",
    "pr_review_comments",
];

/// Read raw payloads out of the doltlite DB. `path` may be either a
/// `.doltlite_db` file or the per-source directory (whose entity db is
/// `entities.doltlite_db`) — both resolve to the same sqlite file via
/// [`db_path_for`].
pub fn parse_api_dir(path: &Path, source_id: &str, range: RawRange<'_>) -> Result<ParsedGithubApi> {
    let db_path = db_path_for(path);
    if !db_path.exists() {
        // No store: this source has never been downloaded. That is
        // the normal state of every source in a freshly scaffolded
        // config, not an error — render nothing and succeed. A store
        // that exists but can't be read still fails, below. See
        // docs/dev/step_protocol.md, "Rendering a source with no data".
        return Ok(ParsedGithubApi::default());
    }
    let (raw, head, changed) = tokio::task::block_in_place(|| {
        let path = db_path.clone();
        tokio::runtime::Handle::current().block_on(async move {
            let Some(db) = RawDb::open_reader(&path, range.pin).await? else {
                return Ok((LoadedRaw::default(), None, None));
            };
            let out = read_everything(&db, range).await;
            // Closed before returning, on the error path too.
            db.close().await;
            out
        })
    })
    .with_context(|| format!("load github db {}", db_path.display()))?;

    let mut parsed = parse_loaded(source_id, raw);
    parsed.head = head;
    // A PR's row id is its bucket key, so a changed PR names itself —
    // gone or not; a changed child names its PR through the row, and a
    // child that went is the driver's to name.
    let forward = changed.map(|changed| {
        let mut out: HashSet<String> = changed.get("pull_requests").cloned().unwrap_or_default();
        for c in &parsed.comments {
            if changed
                .get(c.table)
                .is_some_and(|ids| ids.contains(&c.row_id))
            {
                out.insert(pr_pk(&c.repo_full_name, c.pr_number));
            }
        }
        out
    });
    parsed.render = range.narrow(forward.as_ref());
    if let Some(render) = parsed.render.as_ref() {
        parsed
            .pull_requests
            .retain(|pr| render.contains(&pr.row_id));
        // Comments follow their PR: one left attached to a PR this pass is
        // not rendering would be grouped into a document nobody emits.
        parsed
            .comments
            .retain(|c| render.contains(&pr_pk(&c.repo_full_name, c.pr_number)));
    }
    Ok(parsed)
}

async fn read_everything(
    db: &RawDb,
    range: RawRange<'_>,
) -> Result<(
    LoadedRaw,
    Option<String>,
    Option<HashMap<String, HashSet<String>>>,
)> {
    let pin = db.pin().expect("open_reader returns a pinned handle");
    let raw = LoadedRaw {
        self_identity: db.load_self_identity().await?,
        pull_requests: db.load_pull_requests().await?,
        issue_comments: db.load_children("issue_comments").await?,
        pr_reviews: db.load_children("pr_reviews").await?,
        pr_review_comments: db.load_children("pr_review_comments").await?,
    };
    let changed = changed_rows(db.pool(), range, pin, &TABLES).await?;
    Ok((raw, Some(pin.commit().to_string()), changed))
}

/// The bucket key a PR's rows share: `pull_requests.id`, and the same
/// string composed from any child row's `(repo_full_name, pr_number)`.
fn pr_pk(repo: &str, num: u32) -> String {
    format!("{repo}#{num}")
}

pub fn parse_loaded(source_id: &str, raw: LoadedRaw) -> ParsedGithubApi {
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
        let created_at = p
            .get("created_at")
            .and_then(|v| v.as_str())
            .map(String::from);
        out.pull_requests.push(PullRequestRow {
            uuid: super::ids::pull_request(source_id, &repo, num, created_at.as_deref()).uuid,
            row_id: pr.id,
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
            created_at,
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

    push_issue_comments(source_id, &mut out.comments, raw.issue_comments);
    push_reviews(source_id, &mut out.comments, raw.pr_reviews);
    push_review_comments(source_id, &mut out.comments, raw.pr_review_comments);

    out
}

fn str_field(p: &Value, key: &str) -> String {
    p.get(key).and_then(|v| v.as_str()).unwrap_or("").into()
}

fn push_issue_comments(source_id: &str, out: &mut Vec<CommentRow>, rows: Vec<LoadedChild>) {
    for c in rows {
        let id = c.payload.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        if c.repo_full_name.is_empty() || c.pr_number == 0 || id == 0 {
            continue;
        }
        let p = &c.payload;
        let created_at = str_field(p, "created_at");
        out.push(CommentRow {
            uuid: super::ids::comment(
                source_id,
                &c.repo_full_name,
                ENTITY_ISSUE_COMMENT,
                id,
                Some(&created_at),
            )
            .uuid,
            table: "issue_comments",
            row_id: c.id,
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
            created_at,
            updated_at: p
                .get("updated_at")
                .and_then(|v| v.as_str())
                .map(String::from),
            state: None,
        });
    }
}

fn push_reviews(source_id: &str, out: &mut Vec<CommentRow>, rows: Vec<LoadedChild>) {
    for r in rows {
        let id = r.payload.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
        if r.repo_full_name.is_empty() || r.pr_number == 0 || id == 0 {
            continue;
        }
        let p = &r.payload;
        let created_at = str_field(p, "submitted_at");
        out.push(CommentRow {
            uuid: super::ids::comment(
                source_id,
                &r.repo_full_name,
                ENTITY_PR_REVIEW,
                id,
                Some(&created_at),
            )
            .uuid,
            table: "pr_reviews",
            row_id: r.id,
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
            created_at,
            updated_at: None,
            state: p.get("state").and_then(|v| v.as_str()).map(String::from),
        });
    }
}

fn push_review_comments(source_id: &str, out: &mut Vec<CommentRow>, rows: Vec<LoadedChild>) {
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
        let created_at = str_field(p, "created_at");
        out.push(CommentRow {
            uuid: super::ids::comment(
                source_id,
                &c.repo_full_name,
                ENTITY_PR_REVIEW_COMMENT,
                id,
                Some(&created_at),
            )
            .uuid,
            table: "pr_review_comments",
            row_id: c.id,
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
            created_at,
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
        let parsed =
            parse_api_dir(Path::new("/this/does/not/exist"), "src", RawRange::cold()).unwrap();
        assert!(parsed.pull_requests.is_empty());
        assert!(parsed.comments.is_empty());
        assert!(parsed.self_identity.is_none());
    }
}
