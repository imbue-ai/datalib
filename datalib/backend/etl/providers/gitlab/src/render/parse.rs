//! Parse the GitLab doltlite database written by [`crate::download`] into
//! in-memory rows for the renderer + grid_rows pass. Each discussion
//! (a natively threaded conversation) gets unrolled into one `NoteRow`
//! per note. Notes with `position.new_path` populate the inline section;
//! everything else (including `individual_note: true`) becomes general
//! discussion.

use std::path::Path;

use anyhow::{Context, Result};
use once_cell::sync::Lazy;
use serde_json::Value;
use uuid::Uuid;

use crate::download::db::{db_path_for, LoadedRaw, RawDb};
use crate::download::schema_raw::mr_pk_recipe;

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
    /// The MRs to render this pass — narrowed to the buckets the diff
    /// named, or every MR on a cold start.
    pub merge_requests: Vec<MergeRequestRow>,
    pub notes: Vec<NoteRow>,
    /// MRs the diff reported unchanged, so this run skipped them.
    pub docs_skipped: usize,
    pub scan: ScanResult,
    /// Buckets the diff named whose `merge_requests` row is gone. Empty on
    /// a cold start, which looks at every bucket and so has nothing to
    /// compare against.
    pub vanished_buckets: Vec<String>,
}

/// Result of the `dolt_diff` scan, carried alongside the parsed bag so
/// render can advance the cursor and log the timing.
#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    /// `Some(set)` → render only these `"{project}!{iid}"` buckets.
    /// `None` → cold start: render everything.
    pub changed_buckets: Option<std::collections::HashSet<String>>,
    pub new_head: Option<String>,
    pub scan_elapsed: Option<std::time::Duration>,
}

/// Parse for render, narrowed by `dolt_diff` when a render cursor says
/// where the last run got to. `None` renders everything.
pub fn parse_api_dir(path: &Path, last_render_hash: Option<&str>) -> Result<ParsedGitlabApi> {
    let db_path = db_path_for(path);
    if !db_path.exists() {
        // No store: this source has never been downloaded. That is
        // the normal state of every source in a freshly scaffolded
        // config, not an error — render nothing and succeed. A store
        // that exists but can't be read still fails, below. See
        // docs/dev/step_protocol.md, "Rendering a source with no data".
        return Ok(ParsedGitlabApi::default());
    }
    // One read-only open for the whole parse: the loads, the diff scan and
    // the vanished-bucket probe. Three opens against one doltlite file is
    // the "database is locked" hazard `doltlite_raw::open_reader` warns
    // about — `max_connections` is 1, so a second pool waits on the first
    // rather than failing fast. See #312.
    let (raw, scan, gone) = tokio::task::block_in_place(|| {
        let last = last_render_hash.map(str::to_string);
        let path = db_path.clone();
        tokio::runtime::Handle::current().block_on(async move {
            // `open_reader` pins to HEAD and installs the views, so the
            // loads and the diff below all name one commit. `None` means
            // the store cannot be read at all; gitlab never hands its
            // rendered set to `retain_documents`, so an empty result here
            // deletes nothing.
            let Some(db) = RawDb::open_reader(&path).await? else {
                return Ok(Default::default());
            };
            let pin = db
                .pin()
                .expect("open_reader returns a pinned handle")
                .clone();
            let out = read_everything(&db, last.as_deref(), &pin).await;
            // Closed before returning, on the error path too.
            db.close().await;
            out
        })
    })
    .with_context(|| format!("load gitlab db {}", db_path.display()))?;

    let mut parsed = parse_loaded(raw);
    if let Some(changed) = scan.changed_buckets.as_ref() {
        let before = parsed.merge_requests.len();
        parsed
            .merge_requests
            .retain(|mr| changed.contains(&mr_pk_recipe(&mr.project_full_path, mr.mr_iid)));
        parsed.docs_skipped = before.saturating_sub(parsed.merge_requests.len());
        // Notes follow their MR: one left attached to an MR this pass is
        // not rendering would be grouped into a document nobody emits.
        parsed
            .notes
            .retain(|n| changed.contains(&mr_pk_recipe(&n.project_full_path, n.mr_iid)));
        parsed.vanished_buckets = gone;
    }
    parsed.scan = scan;
    Ok(parsed)
}

/// Everything the parse needs off one open store.
async fn read_everything(
    db: &RawDb,
    last_render_hash: Option<&str>,
    pin: &datalib_etl::pin::Pin,
) -> Result<(LoadedRaw, ScanResult, Vec<String>)> {
    let raw = LoadedRaw {
        self_identity: db.load_self_identity().await?,
        merge_requests: db.load_merge_requests().await?,
        discussions: db.load_discussions().await?,
    };
    let scan = datalib_etl::doltlite_raw::scan_buckets(
        db.pool(),
        last_render_hash,
        pin,
        &datalib_etl::doltlite_raw::DiffScanSpec {
            // `self_identity` is not read by render, so a change to it fans
            // out to nothing.
            global_fanout_tables: &[],
            bucket_query: BUCKET_QUERY,
        },
    )
    .await?;
    let gone = match scan.changed_buckets.as_ref() {
        Some(changed) => {
            datalib_etl::doltlite_raw::buckets_without_rows(
                db.pool(),
                datalib_etl::pin::Reads::At(pin),
                changed,
                &[("merge_requests", "id")],
            )
            .await?
        }
        None => Vec::new(),
    };
    Ok((
        raw,
        ScanResult {
            changed_buckets: scan.changed_buckets,
            new_head: scan.new_head,
            scan_elapsed: scan.scan_elapsed,
        },
        gone,
    ))
}

/// An MR's document is its own row plus its discussions, so either moving
/// re-renders it. `discussions` carries `(project_full_path, mr_iid)`,
/// which composes the same key `merge_requests.id` already holds.
const BUCKET_QUERY: &str = "
    SELECT DISTINCT bucket FROM (
        SELECT coalesce(to_id, from_id) AS bucket
          FROM dolt_diff_merge_requests
         WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
        UNION
        SELECT coalesce(to_project_full_path, from_project_full_path) || '!' ||
               coalesce(to_mr_iid, from_mr_iid)
          FROM dolt_diff_discussions
         WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
    )
    WHERE bucket IS NOT NULL
";

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
        let parsed = parse_api_dir(Path::new("/this/does/not/exist"), None).unwrap();
        assert!(parsed.merge_requests.is_empty());
        assert!(parsed.notes.is_empty());
        assert!(parsed.self_identity.is_none());
    }
}
