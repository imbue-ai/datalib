//! Parse the GitLab event-store JSONL into in-memory rows. Each
//! discussion (a natively threaded conversation) gets unrolled into one
//! `NoteRow` per note. Notes with `position.new_path` populate the inline
//! section; everything else (including `individual_note: true`) becomes
//! general discussion.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use serde_json::Value;
use uuid::Uuid;

pub const ENTITY_SELF: &str = "self_identity";
pub const ENTITY_MR: &str = "merge_request";
pub const ENTITY_DISCUSSION: &str = "discussion";

pub static GITLAB_UUID_NS: Lazy<Uuid> = Lazy::new(|| {
    Uuid::parse_str("c2b91d4b-2080-5e5c-ab34-8f4f3c9e0002").expect("valid gitlab ns uuid")
});

pub fn gitlab_mr_uuid(proj: &str, iid: u32) -> String {
    Uuid::new_v5(
        &GITLAB_UUID_NS,
        format!("gitlab:{proj}:mr:{iid}").as_bytes(),
    )
    .to_string()
}
pub fn gitlab_note_uuid(proj: &str, id: i64) -> String {
    Uuid::new_v5(
        &GITLAB_UUID_NS,
        format!("gitlab:{proj}:note:{id}").as_bytes(),
    )
    .to_string()
}

#[derive(Debug, Clone, Default)]
pub struct GitlabSelfIdentity {
    pub user_id: Option<i64>,
    pub username: Option<String>,
    pub web_url: Option<String>,
    pub raw: Value,
}

#[derive(Debug, Clone)]
pub struct MergeRequestRow {
    pub uuid: String,
    pub project_full_path: String,
    pub mr_iid: u32,
    pub title: String,
    pub body: String,
    pub state: Option<String>,
    pub web_url: Option<String>,
    pub head_sha: Option<String>,
    pub base_sha: Option<String>,
    pub source_branch: Option<String>,
    pub target_branch: Option<String>,
    pub author_username: Option<String>,
    pub created_at: Option<String>,
    pub updated_at: Option<String>,
    pub merged_at: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoteSection {
    General,
    Inline,
}

#[derive(Debug, Clone)]
pub struct NoteRow {
    pub uuid: String,
    pub project_full_path: String,
    pub mr_iid: u32,
    pub kind: &'static str,
    pub section: NoteSection,
    pub external_id: i64,
    /// First-note id in the discussion (parent for threading). `None` if
    /// this note is itself the first in its discussion.
    pub in_reply_to_id: Option<i64>,
    pub discussion_id: String,
    pub author_username: Option<String>,
    pub body: String,
    pub web_url: Option<String>,
    pub path: Option<String>,
    pub line: Option<i64>,
    pub commit_sha: Option<String>,
    pub system: bool,
    pub created_at: String,
    pub updated_at: Option<String>,
}

#[derive(Debug, Default, Clone)]
pub struct ParsedGitlabApi {
    pub self_identity: Option<GitlabSelfIdentity>,
    pub merge_requests: Vec<MergeRequestRow>,
    pub notes: Vec<NoteRow>,
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

pub fn parse_api_dir(api_dir: &Path) -> Result<ParsedGitlabApi> {
    let mut out = ParsedGitlabApi::default();

    // self_identity
    let selves = load_latest_by(api_dir, ENTITY_SELF, |rec| {
        rec.get("user_id")
            .and_then(|v| v.as_i64())
            .map(|n| n.to_string())
            .unwrap_or_default()
    })?;
    if let Some(rec) = selves.into_iter().next() {
        let raw = rec.get("raw").cloned().unwrap_or(Value::Null);
        out.self_identity = Some(GitlabSelfIdentity {
            user_id: rec
                .get("user_id")
                .and_then(|v| v.as_i64())
                .or_else(|| raw.get("id").and_then(|v| v.as_i64())),
            username: rec
                .get("username")
                .and_then(|v| v.as_str())
                .or_else(|| raw.get("username").and_then(|v| v.as_str()))
                .map(String::from),
            web_url: rec
                .get("web_url")
                .and_then(|v| v.as_str())
                .or_else(|| raw.get("web_url").and_then(|v| v.as_str()))
                .map(String::from),
            raw,
        });
    }

    // Merge requests
    for rec in load_latest_by(api_dir, ENTITY_MR, |rec| {
        format!(
            "{}!{}",
            rec.get("project_full_path")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            rec.get("mr_iid").and_then(|v| v.as_i64()).unwrap_or(0)
        )
    })? {
        let raw = rec.get("raw").cloned().unwrap_or(Value::Null);
        let proj = rec
            .get("project_full_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let iid = rec.get("mr_iid").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        if proj.is_empty() || iid == 0 {
            continue;
        }
        let diff_refs = raw.get("diff_refs").cloned().unwrap_or(Value::Null);
        out.merge_requests.push(MergeRequestRow {
            uuid: gitlab_mr_uuid(&proj, iid),
            project_full_path: proj,
            mr_iid: iid,
            title: raw
                .get("title")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            body: raw
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string(),
            state: raw.get("state").and_then(|v| v.as_str()).map(String::from),
            web_url: raw
                .get("web_url")
                .and_then(|v| v.as_str())
                .map(String::from),
            head_sha: diff_refs
                .get("head_sha")
                .and_then(|v| v.as_str())
                .map(String::from),
            base_sha: diff_refs
                .get("base_sha")
                .and_then(|v| v.as_str())
                .map(String::from),
            source_branch: raw
                .get("source_branch")
                .and_then(|v| v.as_str())
                .map(String::from),
            target_branch: raw
                .get("target_branch")
                .and_then(|v| v.as_str())
                .map(String::from),
            author_username: raw
                .get("author")
                .and_then(|a| a.get("username"))
                .and_then(|v| v.as_str())
                .map(String::from),
            created_at: raw
                .get("created_at")
                .and_then(|v| v.as_str())
                .map(String::from),
            updated_at: raw
                .get("updated_at")
                .and_then(|v| v.as_str())
                .map(String::from),
            merged_at: raw
                .get("merged_at")
                .and_then(|v| v.as_str())
                .map(String::from),
        });
    }

    // Discussions → flatten to NoteRows
    for rec in load_latest_by(api_dir, ENTITY_DISCUSSION, |rec| {
        format!(
            "{}!{}#{}",
            rec.get("project_full_path")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            rec.get("mr_iid").and_then(|v| v.as_i64()).unwrap_or(0),
            rec.get("discussion_id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
        )
    })? {
        let raw = rec.get("raw").cloned().unwrap_or(Value::Null);
        let proj = rec
            .get("project_full_path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let iid = rec.get("mr_iid").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let discussion_id = raw
            .get("id")
            .and_then(|v| v.as_str())
            .or_else(|| rec.get("discussion_id").and_then(|v| v.as_str()))
            .unwrap_or("")
            .to_string();
        let individual = raw
            .get("individual_note")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let notes = raw
            .get("notes")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if proj.is_empty() || iid == 0 || notes.is_empty() {
            continue;
        }

        // Figure out the parent (first non-system note id) for threading.
        let parent_id = notes
            .iter()
            .find(|n| !n.get("system").and_then(|v| v.as_bool()).unwrap_or(false))
            .and_then(|n| n.get("id").and_then(|v| v.as_i64()));

        for n in &notes {
            let id = n.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
            if id == 0 {
                continue;
            }
            let system = n.get("system").and_then(|v| v.as_bool()).unwrap_or(false);
            if system {
                // Skip "added/removed label", "marked WIP" etc. system notes.
                continue;
            }
            let position = n.get("position").cloned().unwrap_or(Value::Null);
            let path = position
                .get("new_path")
                .and_then(|v| v.as_str())
                .or_else(|| position.get("old_path").and_then(|v| v.as_str()))
                .map(String::from);
            let line = position
                .get("new_line")
                .and_then(|v| v.as_i64())
                .or_else(|| position.get("old_line").and_then(|v| v.as_i64()));
            let section = if !individual && path.is_some() {
                NoteSection::Inline
            } else {
                NoteSection::General
            };
            let kind = match section {
                NoteSection::Inline => "GitLab Inline Note",
                NoteSection::General => "GitLab Discussion Note",
            };
            let mr_web_url = out
                .merge_requests
                .iter()
                .find(|m| m.project_full_path == proj && m.mr_iid == iid)
                .and_then(|m| m.web_url.clone());
            let web_url = mr_web_url.map(|u| format!("{u}#note_{id}"));
            let in_reply_to_id = match parent_id {
                Some(p) if p != id => Some(p),
                _ => None,
            };
            out.notes.push(NoteRow {
                uuid: gitlab_note_uuid(&proj, id),
                project_full_path: proj.clone(),
                mr_iid: iid,
                kind,
                section,
                external_id: id,
                in_reply_to_id,
                discussion_id: discussion_id.clone(),
                author_username: n
                    .get("author")
                    .and_then(|a| a.get("username"))
                    .and_then(|v| v.as_str())
                    .map(String::from),
                body: n
                    .get("body")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                web_url,
                path,
                line,
                commit_sha: position
                    .get("head_sha")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                system,
                created_at: n
                    .get("created_at")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                updated_at: n
                    .get("updated_at")
                    .and_then(|v| v.as_str())
                    .map(String::from),
            });
        }
    }

    Ok(out)
}
