//! Parse the GitLab doltlite database written by [`datalib_etl_gitlab::ingest`] into
//! in-memory rows for the renderer + grid_rows pass. Each discussion
//! (a natively threaded conversation) gets unrolled into one `NoteRow`
//! per note. Notes with `position.new_path` populate the inline section;
//! everything else (including `individual_note: true`) becomes general
//! discussion.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl_render::inputs::{changed_rows, RawRange};
use once_cell::sync::Lazy;
use serde_json::Value;
use uuid::Uuid;

use datalib_etl_gitlab::ingest::db::{db_path_for, LoadedRaw, RawDb};
use datalib_etl_gitlab::ingest::schema_raw::mr_pk_recipe;

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
    /// `merge_requests.id`, the bucket key every row of this MR shares.
    pub row_id: String,
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
    /// The `discussions` row this note was unrolled from, for the MR's
    /// declaration.
    pub row_id: String,
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
    /// The MRs to render this pass — narrowed to the buckets the diff
    /// named, or every MR on a cold start.
    pub merge_requests: Vec<MergeRequestRow>,
    pub notes: Vec<NoteRow>,
    /// The commit everything was read at.
    pub head: Option<String>,
    /// The buckets to render — `merge_requests.id`s; `None` renders
    /// everything.
    pub render: Option<HashSet<String>>,
}

/// Every table an MR's document reads; the forward scan diffs each.
const TABLES: [&str; 2] = ["merge_requests", "discussions"];

pub fn parse_api_dir(path: &Path, range: RawRange<'_>) -> Result<ParsedGitlabApi> {
    let db_path = db_path_for(path);
    if !db_path.exists() {
        // No store: this source has never been downloaded. That is
        // the normal state of every source in a freshly scaffolded
        // config, not an error — render nothing and succeed. A store
        // that exists but can't be read still fails, below. See
        // docs/dev/step_protocol.md, "Rendering a source with no data".
        return Ok(ParsedGitlabApi::default());
    }
    let (raw, head, changed) = tokio::task::block_in_place(|| {
        let path = db_path.clone();
        tokio::runtime::Handle::current().block_on(async move {
            let Some(db) = RawDb::open_reader_at(&path, range.pin).await? else {
                return Ok((LoadedRaw::default(), None, None));
            };
            let out = read_everything(&db, range).await;
            // Closed before returning, on the error path too.
            db.close().await;
            out
        })
    })
    .with_context(|| format!("load gitlab db {}", db_path.display()))?;

    let mut parsed = parse_loaded(raw);
    parsed.head = head;
    // An MR's row id is its bucket key, so a changed MR names itself —
    // gone or not; a changed discussion names its MR through the row,
    // and a discussion that went is the driver's to name.
    let forward = changed.map(|changed| {
        let mut out: HashSet<String> = changed.get("merge_requests").cloned().unwrap_or_default();
        if let Some(ids) = changed.get("discussions") {
            for n in &parsed.notes {
                if ids.contains(&n.row_id) {
                    out.insert(mr_pk_recipe(&n.project_full_path, n.mr_iid));
                }
            }
        }
        out
    });
    parsed.render = range.narrow(forward.as_ref());
    if let Some(render) = parsed.render.as_ref() {
        parsed
            .merge_requests
            .retain(|mr| render.contains(&mr.row_id));
        // Notes follow their MR: one left attached to an MR this pass is
        // not rendering would be grouped into a document nobody emits.
        parsed
            .notes
            .retain(|n| render.contains(&mr_pk_recipe(&n.project_full_path, n.mr_iid)));
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
    let pin = db.pin().expect("open_reader_at returns a pinned handle");
    let raw = LoadedRaw {
        self_identity: db.load_self_identity().await?,
        merge_requests: db.load_merge_requests().await?,
        discussions: db.load_discussions().await?,
    };
    let changed = changed_rows(db.pool(), range, pin, &TABLES).await?;
    Ok((raw, Some(pin.commit().to_string()), changed))
}

pub fn parse_loaded(raw: LoadedRaw) -> ParsedGitlabApi {
    let mut out = ParsedGitlabApi::default();

    if let Some(s) = raw.self_identity {
        out.self_identity = Some(GitlabSelfIdentity {
            user_id: s.get("id").and_then(|v| v.as_i64()),
            username: s.get("username").and_then(|v| v.as_str()).map(String::from),
            web_url: s.get("web_url").and_then(|v| v.as_str()).map(String::from),
            raw: s,
        });
    }

    for mr in raw.merge_requests {
        let proj = mr.project_full_path;
        let iid = mr.mr_iid;
        if proj.is_empty() || iid == 0 {
            continue;
        }
        let p = &mr.payload;
        let diff_refs = p.get("diff_refs");
        out.merge_requests.push(MergeRequestRow {
            uuid: gitlab_mr_uuid(&proj, iid),
            row_id: mr.id,
            project_full_path: proj,
            mr_iid: iid,
            title: p.get("title").and_then(|v| v.as_str()).unwrap_or("").into(),
            body: p
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .into(),
            state: p.get("state").and_then(|v| v.as_str()).map(String::from),
            web_url: p.get("web_url").and_then(|v| v.as_str()).map(String::from),
            head_sha: diff_refs
                .and_then(|d| d.get("head_sha"))
                .and_then(|v| v.as_str())
                .map(String::from),
            base_sha: diff_refs
                .and_then(|d| d.get("base_sha"))
                .and_then(|v| v.as_str())
                .map(String::from),
            source_branch: p
                .get("source_branch")
                .and_then(|v| v.as_str())
                .map(String::from),
            target_branch: p
                .get("target_branch")
                .and_then(|v| v.as_str())
                .map(String::from),
            author_username: p
                .get("author")
                .and_then(|a| a.get("username"))
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

    // Discussions → flatten to NoteRows.
    for d in raw.discussions {
        let row_id = d.id;
        let proj = d.project_full_path;
        let iid = d.mr_iid;
        let payload = d.payload;
        let discussion_id = d.discussion_id;
        let individual = payload
            .get("individual_note")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let notes = payload
            .get("notes")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if proj.is_empty() || iid == 0 || notes.is_empty() {
            continue;
        }

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
                row_id: row_id.clone(),
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
                body: n.get("body").and_then(|v| v.as_str()).unwrap_or("").into(),
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
                    .into(),
                updated_at: n
                    .get("updated_at")
                    .and_then(|v| v.as_str())
                    .map(String::from),
            });
        }
    }

    out
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
        let parsed = parse_api_dir(Path::new("/this/does/not/exist"), RawRange::cold()).unwrap();
        assert!(parsed.merge_requests.is_empty());
        assert!(parsed.notes.is_empty());
        assert!(parsed.self_identity.is_none());
    }
}
