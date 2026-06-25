//! Build the `grid_rows` sidecar for one GitLab MR document.

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};

use anyhow::Result;
use frankweiler_schema::grid_rows::GridRow;
use serde_json::Value;

use super::parse::{MergeRequestRow, NoteRow, NoteSection};

pub const RENDER_VERSION: u32 = 1;

fn canonicalize(v: &Value) -> Value {
    match v {
        Value::Object(m) => {
            let mut pairs: Vec<_> = m.iter().collect();
            pairs.sort_by(|a, b| a.0.cmp(b.0));
            let mut out = serde_json::Map::with_capacity(pairs.len());
            for (k, val) in pairs {
                out.insert(k.clone(), canonicalize(val));
            }
            Value::Object(out)
        }
        Value::Array(a) => Value::Array(a.iter().map(canonicalize).collect()),
        other => other.clone(),
    }
}

fn note_json(n: &NoteRow) -> Value {
    serde_json::json!({
        "uuid": n.uuid,
        "kind": n.kind,
        "section": format!("{:?}", n.section),
        "external_id": n.external_id,
        "in_reply_to_id": n.in_reply_to_id,
        "discussion_id": n.discussion_id,
        "author_username": n.author_username,
        "body": n.body,
        "path": n.path,
        "line": n.line,
        "commit_sha": n.commit_sha,
        "created_at": n.created_at,
        "updated_at": n.updated_at,
    })
}

fn mr_json(mr: &MergeRequestRow) -> Value {
    serde_json::json!({
        "uuid": mr.uuid,
        "project": mr.project_full_path,
        "mr_iid": mr.mr_iid,
        "title": mr.title,
        "body": mr.body,
        "state": mr.state,
        "head_sha": mr.head_sha,
        "base_sha": mr.base_sha,
        "merged_at": mr.merged_at,
        "updated_at": mr.updated_at,
    })
}

pub fn fingerprint_for_mr(mr: &MergeRequestRow, notes: &[NoteRow]) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    RENDER_VERSION.hash(&mut h);
    serde_json::to_string(&canonicalize(&mr_json(mr)))
        .unwrap_or_default()
        .hash(&mut h);
    let mut sorted: Vec<&NoteRow> = notes.iter().collect();
    sorted.sort_by_key(|n| n.external_id);
    for n in sorted {
        serde_json::to_string(&canonicalize(&note_json(n)))
            .unwrap_or_default()
            .hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

fn ordered_notes(notes: &[NoteRow]) -> Vec<&NoteRow> {
    let mut general: Vec<&NoteRow> = notes
        .iter()
        .filter(|n| n.section == NoteSection::General)
        .collect();
    general.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then(a.external_id.cmp(&b.external_id))
    });

    let inline: Vec<&NoteRow> = notes
        .iter()
        .filter(|n| n.section == NoteSection::Inline)
        .collect();
    let mut groups: BTreeMap<(String, i64), Vec<&NoteRow>> = BTreeMap::new();
    let mut keys: BTreeSet<(String, i64)> = BTreeSet::new();
    for n in &inline {
        let key = (
            n.path.clone().unwrap_or_else(|| "unknown".into()),
            n.line.unwrap_or(0),
        );
        keys.insert(key.clone());
        groups.entry(key).or_default().push(n);
    }
    let mut out: Vec<&NoteRow> = Vec::new();
    out.extend(general);
    for k in keys {
        let mut bucket = groups.remove(&k).unwrap_or_default();
        bucket.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                .then(a.external_id.cmp(&b.external_id))
        });
        out.extend(bucket);
    }
    out
}

pub fn rows_for_mr(stanza: &str, mr: &MergeRequestRow, notes: &[NoteRow]) -> Result<Vec<GridRow>> {
    let qmd = super::render::mr_qmd_path_rel(stanza, &mr.project_full_path, mr.mr_iid);
    let entire_chat = format!("/chat/{}", mr.uuid);

    let mut rows: Vec<GridRow> = Vec::new();
    rows.push(
        GridRow::builder()
            .uuid(mr.uuid.clone())
            .provider("gitlab")
            .kind("GitLab MR")
            .source_label("GitLab")
            .when_ts(mr.updated_at.clone().or_else(|| mr.created_at.clone()))
            .author(mr.author_username.clone())
            .project(Some(mr.project_full_path.clone()))
            .conversation_name(Some(mr.title.clone()))
            .conversation_uuid(mr.uuid.clone())
            .entire_chat(entire_chat.clone())
            .text(if mr.body.is_empty() {
                mr.title.clone()
            } else {
                format!("{}\n\n{}", mr.title, mr.body)
            })
            .qmd_path(Some(qmd.clone()))
            .source_url(mr.web_url.clone())
            .git_sha(mr.head_sha.clone())
            .external_id(Some(mr.mr_iid.to_string()))
            .markdown_uuid(Some(mr.uuid.clone()))
            .build()?,
    );

    for (idx, n) in ordered_notes(notes).into_iter().enumerate() {
        rows.push(
            GridRow::builder()
                .uuid(n.uuid.clone())
                .provider("gitlab")
                .kind(n.kind)
                .source_label("GitLab")
                .when_ts(Some(n.created_at.clone()))
                .author(n.author_username.clone())
                .project(Some(mr.project_full_path.clone()))
                .conversation_name(Some(mr.title.clone()))
                .conversation_uuid(mr.uuid.clone())
                .message_index(Some(idx as i64))
                .entire_chat(entire_chat.clone())
                .text(n.body.clone())
                .qmd_path(Some(qmd.clone()))
                .source_url(n.web_url.clone())
                .git_sha(n.commit_sha.clone())
                .external_id(Some(n.external_id.to_string()))
                .markdown_uuid(Some(mr.uuid.clone()))
                .build()?,
        );
    }
    Ok(rows)
}
