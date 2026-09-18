//! Build the `grid_rows` for one GitHub PR document.

use std::collections::{BTreeMap, BTreeSet};

use datalib_schema::grid_rows::GridRow;
use datalib_schema::problems::ProblemRow;
use datalib_schema::providers::Provider;

use super::parse::{CommentRow, CommentSection, PullRequestRow};

pub const RENDER_VERSION: u32 = 1;

/// Sort comments into rendered order (matches `render.rs`).
fn ordered_comments(comments: &[CommentRow]) -> Vec<&CommentRow> {
    let mut reviews: Vec<&CommentRow> = comments
        .iter()
        .filter(|c| c.section == CommentSection::Review)
        .collect();
    reviews.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then(a.external_id.cmp(&b.external_id))
    });
    let mut general: Vec<&CommentRow> = comments
        .iter()
        .filter(|c| c.section == CommentSection::General)
        .collect();
    general.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then(a.external_id.cmp(&b.external_id))
    });

    // Inline grouped by (path, line) anchor; replies inherit parent's anchor.
    let inline: Vec<&CommentRow> = comments
        .iter()
        .filter(|c| c.section == CommentSection::Inline)
        .collect();
    let mut anchor_for_id: std::collections::HashMap<i64, (String, i64)> = Default::default();
    for c in &inline {
        if c.in_reply_to_id.is_none() {
            anchor_for_id.insert(
                c.external_id,
                (
                    c.path.clone().unwrap_or_else(|| "unknown".into()),
                    c.line.unwrap_or(0),
                ),
            );
        }
    }
    let mut groups: BTreeMap<(String, i64), Vec<&CommentRow>> = BTreeMap::new();
    let mut keys: BTreeSet<(String, i64)> = BTreeSet::new();
    for c in &inline {
        let anchor = c
            .in_reply_to_id
            .and_then(|p| anchor_for_id.get(&p).cloned())
            .unwrap_or_else(|| {
                (
                    c.path.clone().unwrap_or_else(|| "unknown".into()),
                    c.line.unwrap_or(0),
                )
            });
        keys.insert(anchor.clone());
        groups.entry(anchor).or_default().push(c);
    }
    let mut out: Vec<&CommentRow> = Vec::new();
    out.extend(reviews);
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
pub fn rows_for_pr(
    pr: &PullRequestRow,
    comments: &[CommentRow],
    stanza: &str,
    problems: &mut Vec<ProblemRow>,
) -> Vec<GridRow> {
    let qmd = super::render::pr_qmd_path_rel(stanza, &pr.repo_full_name, pr.pr_number);
    let entire_chat = format!("/chat/{}", pr.uuid);

    let mut rows: Vec<GridRow> = Vec::new();
    rows.extend(
        GridRow::builder()
            .uuid(pr.uuid.clone())
            .provider(Provider::Github)
            .kind("GitHub PR")
            .source_label("GitHub")
            .is_document(true)
            .created_at(pr.created_at.clone())
            .modified_at(pr.updated_at.clone())
            .author(pr.user_login.clone())
            .project(Some(pr.repo_full_name.clone()))
            .conversation_name(Some(pr.title.clone()))
            .conversation_uuid(pr.uuid.clone())
            .entire_chat(entire_chat.clone())
            .text(if pr.body.is_empty() {
                pr.title.clone()
            } else {
                format!("{}\n\n{}", pr.title, pr.body)
            })
            .qmd_path(Some(qmd.clone()))
            .source_url(pr.html_url.clone())
            .git_sha(pr.head_sha.clone())
            .upstream_id(Some(pr.pr_number.to_string()))
            .upstream_entity_kind(Some(crate::render::parse::ENTITY_PR.to_string()))
            .markdown_uuid(Some(pr.uuid.clone()))
            .build_or_record(stanza, &pr.uuid, RENDER_VERSION, problems),
    );

    for (idx, c) in ordered_comments(comments).into_iter().enumerate() {
        rows.extend(
            GridRow::builder()
                .uuid(c.uuid.clone())
                .provider(Provider::Github)
                .kind(c.kind)
                .source_label("GitHub")
                .created_at(Some(c.created_at.clone()))
                // GitHub stamps `updated_at` on every comment, equal to
                // `created_at` until it is edited; only an edit is a change.
                .modified_at(c.updated_at.clone().filter(|u| *u != c.created_at))
                .author(c.user_login.clone())
                .project(Some(pr.repo_full_name.clone()))
                .conversation_name(Some(pr.title.clone()))
                .conversation_uuid(pr.uuid.clone())
                .message_index(Some(idx as i64))
                .entire_chat(entire_chat.clone())
                .text(c.body.clone())
                .qmd_path(Some(qmd.clone()))
                .source_url(c.html_url.clone())
                .git_sha(c.commit_id.clone())
                .upstream_id(Some(c.external_id.to_string()))
                // `issue_comment` / `pr_review` / `pr_review_comment` —
                // the three live in separate GitHub API namespaces and
                // their numeric ids overlap freely, so the bare id is
                // not a usable backpointer without this.
                .upstream_entity_kind(Some(c.section.entity().to_string()))
                .markdown_uuid(Some(pr.uuid.clone()))
                .build_or_record(stanza, &pr.uuid, RENDER_VERSION, problems),
        );
    }
    rows
}
