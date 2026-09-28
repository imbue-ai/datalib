//! Parse the GitLab doltlite database written by [`datalib_etl_gitlab::ingest`] into
//! in-memory rows for the renderer + grid_rows pass. Each discussion
//! (a natively threaded conversation) gets unrolled into one `Comment`
//! per note. Notes with `position.new_path` populate the inline section;
//! everything else (including `individual_note: true`) becomes general
//! discussion.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl_render::inputs::{changed_rows, RawRange};
use serde_json::Value;

use datalib_etl_gitlab::ingest::db::{db_path_for, LoadedRaw, RawDb};
use datalib_etl_gitlab::ingest::schema_raw::mr_pk_recipe;

use datalib_etl_forge_render_common::{ChangeRequest, Comment, Parsed, Section};

use super::ids::KIND_NOTE;

/// Every table an MR's document reads; the forward scan diffs each.
const TABLES: [&str; 2] = ["merge_requests", "discussions"];

pub fn parse_api_dir(path: &Path, source_id: &str, range: RawRange<'_>) -> Result<Parsed> {
    let db_path = db_path_for(path);
    if !db_path.exists() {
        // No store: this source has never been downloaded. That is
        // the normal state of every source in a freshly scaffolded
        // config, not an error — render nothing and succeed. A store
        // that exists but can't be read still fails, below. See
        // docs/dev/step_protocol.md, "Rendering a source with no data".
        return Ok(Parsed::default());
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
    .with_context(|| format!("load gitlab db {}", db_path.display()))?;

    let mut parsed = parse_loaded(source_id, raw);
    parsed.head = head;
    parsed.narrow(TABLES[0], changed, range);
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
        merge_requests: db.load_merge_requests().await?,
        discussions: db.load_discussions().await?,
    };
    let changed = changed_rows(db.pool(), range, pin, &TABLES).await?;
    Ok((raw, Some(pin.commit().to_string()), changed))
}

pub fn parse_loaded(source_id: &str, raw: LoadedRaw) -> Parsed {
    let mut out = Parsed::default();

    for mr in raw.merge_requests {
        let proj = mr.project_full_path;
        let iid = mr.mr_iid;
        if proj.is_empty() || iid == 0 {
            continue;
        }
        let p = &mr.payload;
        let diff_refs = p.get("diff_refs");
        let created_at = opt_str(p, "created_at");
        out.change_requests.push(ChangeRequest {
            uuid: super::ids::merge_request(source_id, &proj, iid, created_at.as_deref()).uuid,
            row_id: mr.id,
            container: proj,
            number: iid,
            title: str_field(p, "title"),
            body: str_field(p, "description"),
            state: opt_str(p, "state"),
            url: opt_str(p, "web_url"),
            head_sha: diff_refs.and_then(|d| opt_str(d, "head_sha")),
            base_sha: diff_refs.and_then(|d| opt_str(d, "base_sha")),
            from_ref: opt_str(p, "source_branch"),
            to_ref: opt_str(p, "target_branch"),
            author: username(p),
            created_at,
            updated_at: opt_str(p, "updated_at"),
            merged_at: opt_str(p, "merged_at"),
        });
    }

    // Each discussion unrolls into one comment per note.
    for d in raw.discussions {
        let proj = d.project_full_path;
        let iid = d.mr_iid;
        let payload = d.payload;
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
        let is_system = |n: &Value| n.get("system").and_then(|v| v.as_bool()).unwrap_or(false);
        // The first note a person wrote is the thread every other note
        // replies to.
        let parent_id = notes
            .iter()
            .find(|n| !is_system(n))
            .and_then(|n| n.get("id").and_then(|v| v.as_i64()));
        let mr_web_url = out
            .change_requests
            .iter()
            .find(|m| m.container == proj && m.number == iid)
            .and_then(|m| m.url.clone());

        for n in &notes {
            let id = n.get("id").and_then(|v| v.as_i64()).unwrap_or(0);
            if id == 0 || is_system(n) {
                continue;
            }
            let position = n.get("position").cloned().unwrap_or(Value::Null);
            let path = opt_str(&position, "new_path").or_else(|| opt_str(&position, "old_path"));
            let line = position
                .get("new_line")
                .and_then(|v| v.as_i64())
                .or_else(|| position.get("old_line").and_then(|v| v.as_i64()));
            let section = if !individual && path.is_some() {
                Section::Inline
            } else {
                Section::General
            };
            let created_at = str_field(n, "created_at");
            out.comments.push(Comment {
                uuid: super::ids::note(source_id, &proj, id, Some(&created_at)).uuid,
                table: "discussions",
                row_id: d.id.clone(),
                parent_row_id: mr_pk_recipe(&proj, iid),
                kind: match section {
                    Section::Inline => "GitLab Inline Note",
                    _ => "GitLab Discussion Note",
                },
                entity_kind: KIND_NOTE,
                section,
                external_id: id,
                in_reply_to_id: parent_id.filter(|p| *p != id),
                author: username(n),
                body: str_field(n, "body"),
                url: mr_web_url.as_ref().map(|u| format!("{u}#note_{id}")),
                path,
                line,
                commit_sha: opt_str(&position, "head_sha"),
                created_at,
                updated_at: opt_str(n, "updated_at"),
                state: None,
            });
        }
    }

    out
}

fn str_field(p: &Value, key: &str) -> String {
    p.get(key).and_then(|v| v.as_str()).unwrap_or("").into()
}

fn opt_str(p: &Value, key: &str) -> Option<String> {
    p.get(key).and_then(|v| v.as_str()).map(String::from)
}

fn username(p: &Value) -> Option<String> {
    p.get("author").and_then(|a| opt_str(a, "username"))
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
        assert!(parsed.change_requests.is_empty());
        assert!(parsed.comments.is_empty());
    }
}
