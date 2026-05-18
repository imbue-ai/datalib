//! Parse the GitHub event-store JSONL into in-memory rows for the
//! renderer + grid_rows pass. Port of
//! `src/ingest/providers/github/parse.py`, adapted to the
//! single-document-per-PR model: each PR's `issue_comments`,
//! `pr_reviews`, and `pr_review_comments` collapse into one
//! `CommentRow` stream sorted (per render) by section, then by file/line,
//! then chronologically within a thread.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use serde_json::Value;
use uuid::Uuid;

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
    pub pull_requests: Vec<PullRequestRow>,
    pub comments: Vec<CommentRow>,
}

fn read_jsonl(path: &Path) -> Result<Vec<Value>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    for (i, line) in BufReader::new(f).lines().enumerate() {
        let line = line.with_context(|| format!("read {}:{}", path.display(), i + 1))?;
        if line.trim().is_empty() {
            continue;
        }
        let v: Value = serde_json::from_str(&line)
            .with_context(|| format!("parse {}:{}", path.display(), i + 1))?;
        out.push(v);
    }
    Ok(out)
}

/// Load the latest record per key across `created/` then `updated/` (the
/// `updated/` stream shadows by re-insertion).
fn load_latest_by(
    api_dir: &Path,
    entity: &str,
    key_of: impl Fn(&Value) -> String,
) -> Result<Vec<Value>> {
    let mut latest: HashMap<String, Value> = HashMap::new();
    for stream in ["created", "updated"] {
        let p = api_dir.join(entity).join(stream).join("events.jsonl");
        for rec in read_jsonl(&p)? {
            let k = key_of(&rec);
            if !k.is_empty() {
                latest.insert(k, rec);
            }
        }
    }
    Ok(latest.into_values().collect())
}

fn str_of<'a>(v: &'a Value, k: &str) -> Option<&'a str> {
    v.get(k).and_then(|x| x.as_str())
}
fn str_or_raw<'a>(rec: &'a Value, k: &str) -> Option<&'a str> {
    str_of(rec, k).or_else(|| {
        rec.get("raw")
            .and_then(|r| r.get(k))
            .and_then(|v| v.as_str())
    })
}

pub fn parse_api_dir(api_dir: &Path) -> Result<ParsedGithubApi> {
    let mut out = ParsedGithubApi::default();

    // self_identity
    let selves = load_latest_by(api_dir, ENTITY_SELF, |rec| {
        rec.get("user_id")
            .and_then(|v| v.as_i64())
            .map(|n| n.to_string())
            .unwrap_or_default()
    })?;
    if let Some(rec) = selves.into_iter().next() {
        let raw = rec.get("raw").cloned().unwrap_or(Value::Null);
        out.self_identity = Some(GithubSelfIdentity {
            user_id: rec
                .get("user_id")
                .and_then(|v| v.as_i64())
                .or_else(|| raw.get("id").and_then(|v| v.as_i64())),
            login: rec
                .get("login")
                .and_then(|v| v.as_str())
                .or_else(|| raw.get("login").and_then(|v| v.as_str()))
                .map(String::from),
            html_url: rec
                .get("html_url")
                .and_then(|v| v.as_str())
                .or_else(|| raw.get("html_url").and_then(|v| v.as_str()))
                .map(String::from),
            raw,
        });
    }

    // Pull requests
    for rec in load_latest_by(api_dir, ENTITY_PR, |rec| {
        format!(
            "{}#{}",
            rec.get("repo_full_name")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            rec.get("pr_number").and_then(|v| v.as_i64()).unwrap_or(0)
        )
    })? {
        let raw = rec.get("raw").cloned().unwrap_or(Value::Null);
        let repo = rec
            .get("repo_full_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let num = rec
            .get("pr_number")
            .and_then(|v| v.as_u64())
            .or_else(|| raw.get("number").and_then(|v| v.as_u64()))
            .unwrap_or(0) as u32;
        if repo.is_empty() || num == 0 {
            continue;
        }
        out.pull_requests.push(PullRequestRow {
            uuid: github_pr_uuid(&repo, num),
            repo_full_name: repo,
            pr_number: num,
            title: raw
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            body: raw
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            state: str_or_raw(&rec, "state").map(String::from),
            html_url: str_or_raw(&rec, "html_url").map(String::from),
            head_sha: rec
                .get("head_sha")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    raw.get("head")
                        .and_then(|h| h.get("sha"))
                        .and_then(|v| v.as_str())
                })
                .map(String::from),
            base_sha: rec
                .get("base_sha")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    raw.get("base")
                        .and_then(|h| h.get("sha"))
                        .and_then(|v| v.as_str())
                })
                .map(String::from),
            head_ref: rec
                .get("head_ref")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    raw.get("head")
                        .and_then(|h| h.get("ref"))
                        .and_then(|v| v.as_str())
                })
                .map(String::from),
            base_ref: rec
                .get("base_ref")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    raw.get("base")
                        .and_then(|h| h.get("ref"))
                        .and_then(|v| v.as_str())
                })
                .map(String::from),
            user_login: raw
                .get("user")
                .and_then(|u| u.get("login"))
                .and_then(|v| v.as_str())
                .map(String::from),
            created_at: raw
                .get("created_at")
                .and_then(|v| v.as_str())
                .map(String::from),
            updated_at: rec
                .get("updated_at")
                .and_then(|v| v.as_str())
                .or_else(|| raw.get("updated_at").and_then(|v| v.as_str()))
                .map(String::from),
            merged_at: str_or_raw(&rec, "merged_at").map(String::from),
        });
    }

    // Issue comments → General section
    for rec in load_latest_by(api_dir, ENTITY_ISSUE_COMMENT, |rec| {
        format!(
            "{}#{}",
            rec.get("repo_full_name")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            rec.get("comment_id").and_then(|v| v.as_i64()).unwrap_or(0)
        )
    })? {
        let raw = rec.get("raw").cloned().unwrap_or(Value::Null);
        let repo = rec
            .get("repo_full_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let num = rec.get("pr_number").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let id = rec
            .get("comment_id")
            .and_then(|v| v.as_i64())
            .or_else(|| raw.get("id").and_then(|v| v.as_i64()))
            .unwrap_or(0);
        if repo.is_empty() || num == 0 || id == 0 {
            continue;
        }
        out.comments.push(CommentRow {
            uuid: github_issue_comment_uuid(&repo, id),
            repo_full_name: repo,
            pr_number: num,
            kind: "GitHub PR Comment",
            section: CommentSection::General,
            external_id: id,
            in_reply_to_id: None,
            user_login: rec
                .get("user_login")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    raw.get("user")
                        .and_then(|u| u.get("login"))
                        .and_then(|v| v.as_str())
                })
                .map(String::from),
            body: raw
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            html_url: str_or_raw(&rec, "html_url").map(String::from),
            path: None,
            line: None,
            commit_id: None,
            created_at: str_or_raw(&rec, "created_at").unwrap_or("").to_string(),
            updated_at: str_or_raw(&rec, "updated_at").map(String::from),
            state: None,
        });
    }

    // PR reviews → Review section
    for rec in load_latest_by(api_dir, ENTITY_PR_REVIEW, |rec| {
        format!(
            "{}#{}",
            rec.get("repo_full_name")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            rec.get("review_id").and_then(|v| v.as_i64()).unwrap_or(0)
        )
    })? {
        let raw = rec.get("raw").cloned().unwrap_or(Value::Null);
        let repo = rec
            .get("repo_full_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let num = rec.get("pr_number").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let id = rec
            .get("review_id")
            .and_then(|v| v.as_i64())
            .or_else(|| raw.get("id").and_then(|v| v.as_i64()))
            .unwrap_or(0);
        if repo.is_empty() || num == 0 || id == 0 {
            continue;
        }
        let body = raw
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let state = rec
            .get("state")
            .and_then(|v| v.as_str())
            .or_else(|| raw.get("state").and_then(|v| v.as_str()))
            .map(String::from);
        out.comments.push(CommentRow {
            uuid: github_review_uuid(&repo, id),
            repo_full_name: repo,
            pr_number: num,
            kind: "GitHub Review",
            section: CommentSection::Review,
            external_id: id,
            in_reply_to_id: None,
            user_login: rec
                .get("user_login")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    raw.get("user")
                        .and_then(|u| u.get("login"))
                        .and_then(|v| v.as_str())
                })
                .map(String::from),
            body,
            html_url: str_or_raw(&rec, "html_url").map(String::from),
            path: None,
            line: None,
            commit_id: rec
                .get("commit_id")
                .and_then(|v| v.as_str())
                .or_else(|| raw.get("commit_id").and_then(|v| v.as_str()))
                .map(String::from),
            created_at: rec
                .get("submitted_at")
                .and_then(|v| v.as_str())
                .or_else(|| raw.get("submitted_at").and_then(|v| v.as_str()))
                .unwrap_or("")
                .to_string(),
            updated_at: None,
            state,
        });
    }

    // PR review comments → Inline section
    for rec in load_latest_by(api_dir, ENTITY_PR_REVIEW_COMMENT, |rec| {
        format!(
            "{}#{}",
            rec.get("repo_full_name")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            rec.get("comment_id").and_then(|v| v.as_i64()).unwrap_or(0)
        )
    })? {
        let raw = rec.get("raw").cloned().unwrap_or(Value::Null);
        let repo = rec
            .get("repo_full_name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let num = rec.get("pr_number").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let id = rec
            .get("comment_id")
            .and_then(|v| v.as_i64())
            .or_else(|| raw.get("id").and_then(|v| v.as_i64()))
            .unwrap_or(0);
        if repo.is_empty() || num == 0 || id == 0 {
            continue;
        }
        let in_reply_to = rec
            .get("in_reply_to_id")
            .and_then(|v| v.as_i64())
            .or_else(|| raw.get("in_reply_to_id").and_then(|v| v.as_i64()));
        out.comments.push(CommentRow {
            uuid: github_review_comment_uuid(&repo, id),
            repo_full_name: repo,
            pr_number: num,
            kind: "GitHub Review Comment",
            section: CommentSection::Inline,
            external_id: id,
            in_reply_to_id: in_reply_to,
            user_login: rec
                .get("user_login")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    raw.get("user")
                        .and_then(|u| u.get("login"))
                        .and_then(|v| v.as_str())
                })
                .map(String::from),
            body: raw
                .get("body")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            html_url: str_or_raw(&rec, "html_url").map(String::from),
            path: rec
                .get("path")
                .and_then(|v| v.as_str())
                .or_else(|| raw.get("path").and_then(|v| v.as_str()))
                .map(String::from),
            line: rec
                .get("line")
                .and_then(|v| v.as_i64())
                .or_else(|| rec.get("original_line").and_then(|v| v.as_i64()))
                .or_else(|| raw.get("line").and_then(|v| v.as_i64())),
            commit_id: rec
                .get("commit_id")
                .and_then(|v| v.as_str())
                .or_else(|| rec.get("original_commit_id").and_then(|v| v.as_str()))
                .map(String::from),
            created_at: str_or_raw(&rec, "created_at").unwrap_or("").to_string(),
            updated_at: str_or_raw(&rec, "updated_at").map(String::from),
            state: None,
        });
    }

    Ok(out)
}
