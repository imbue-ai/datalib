//! The `grid_rows` for one change request's document: its own row, then
//! one per comment in the order the document shows them. A row that
//! will not validate is dropped and recorded on `problems` rather than
//! failing the source's render — see `GridRowBuilder::build_or_record`.

use datalib_id::composite_key;
use datalib_schema::grid_rows::GridRow;
use datalib_schema::problems::ProblemRow;

use crate::{ordered, ChangeRequest, Comment, ForgeProfile};

pub(crate) fn rows_for(
    profile: &ForgeProfile,
    cr: &ChangeRequest,
    comments: &[&Comment],
    stanza: &str,
    qmd: &str,
    problems: &mut Vec<ProblemRow>,
) -> Vec<GridRow> {
    let entire_chat = format!("/chat/{}", cr.uuid);
    let version = profile.render_version;

    let mut rows: Vec<GridRow> = Vec::new();
    rows.extend(
        GridRow::builder()
            .uuid(cr.uuid.clone())
            .provider(profile.provider)
            .kind(profile.doc_kind)
            .source_label(profile.source_label)
            .is_document(true)
            .created_at(cr.created_at.clone())
            .modified_at(cr.updated_at.clone())
            .author(cr.author.clone())
            .project(Some(cr.container.clone()))
            .conversation_name(Some(cr.title.clone()))
            .conversation_uuid(cr.uuid.clone())
            .entire_chat(entire_chat.clone())
            .body(if cr.body.is_empty() {
                cr.title.clone()
            } else {
                format!("{}\n\n{}", cr.title, cr.body)
            })
            .qmd_path(Some(qmd.to_string()))
            .source_url(cr.url.clone())
            .git_sha(cr.head_sha.clone())
            .upstream_id(Some(composite_key(&[
                &cr.container,
                &cr.number.to_string(),
            ])))
            .upstream_entity_kind(Some(profile.doc_entity_kind.to_string()))
            .markdown_uuid(Some(cr.uuid.clone()))
            .build_or_record(stanza, &cr.uuid, version, problems),
    );

    for (idx, c) in ordered(comments).flat().enumerate() {
        rows.extend(
            GridRow::builder()
                .uuid(c.uuid.clone())
                .provider(profile.provider)
                .kind(c.kind)
                .source_label(profile.source_label)
                .created_at(Some(c.created_at.clone()))
                // Both forges stamp `updated_at` on every comment, equal
                // to `created_at` until it is edited; only an edit is a
                // change.
                .modified_at(c.updated_at.clone().filter(|u| *u != c.created_at))
                .author(c.author.clone())
                .project(Some(cr.container.clone()))
                .conversation_name(Some(cr.title.clone()))
                .conversation_uuid(cr.uuid.clone())
                .message_index(Some(idx as i64))
                .entire_chat(entire_chat.clone())
                .body(c.body.clone())
                .qmd_path(Some(qmd.to_string()))
                .source_url(c.url.clone())
                .git_sha(c.commit_sha.clone())
                .upstream_id(Some(composite_key(&[
                    &cr.container,
                    &c.external_id.to_string(),
                ])))
                .upstream_entity_kind(Some(c.entity_kind.to_string()))
                .markdown_uuid(Some(cr.uuid.clone()))
                .build_or_record(stanza, &cr.uuid, version, problems),
        );
    }
    rows
}
