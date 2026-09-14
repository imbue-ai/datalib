//! Build the `grid_rows` for one GitLab MR document.

use std::collections::{BTreeMap, BTreeSet};

use datalib_schema::grid_rows::GridRow;
use datalib_schema::providers::Provider;
use datalib_schema::render_problems::RenderProblemRow;

use super::parse::{MergeRequestRow, NoteRow, NoteSection};

pub const RENDER_VERSION: u32 = 1;

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

/// A row that will not validate is dropped and recorded on `problems`
/// rather than failing the whole source's render — see
/// `GridRowBuilder::build_or_record`.
pub fn rows_for_mr(
    stanza: &str,
    mr: &MergeRequestRow,
    notes: &[NoteRow],
    problems: &mut Vec<RenderProblemRow>,
) -> Vec<GridRow> {
    let qmd = super::render::mr_qmd_path_rel(stanza, &mr.project_full_path, mr.mr_iid);
    let entire_chat = format!("/chat/{}", mr.uuid);

    let mut rows: Vec<GridRow> = Vec::new();
    rows.extend(
        GridRow::builder()
            .uuid(mr.uuid.clone())
            .provider(Provider::Gitlab)
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
            .upstream_id(Some(mr.mr_iid.to_string()))
            .upstream_entity_kind(Some("merge_request".to_string()))
            .markdown_uuid(Some(mr.uuid.clone()))
            .build_or_record(stanza, &mr.uuid, RENDER_VERSION, problems),
    );

    for (idx, n) in ordered_notes(notes).into_iter().enumerate() {
        rows.extend(
            GridRow::builder()
                .uuid(n.uuid.clone())
                .provider(Provider::Gitlab)
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
                .upstream_id(Some(n.external_id.to_string()))
                .upstream_entity_kind(Some("note".to_string()))
                .markdown_uuid(Some(mr.uuid.clone()))
                .build_or_record(stanza, &mr.uuid, RENDER_VERSION, problems),
        );
    }
    rows
}
