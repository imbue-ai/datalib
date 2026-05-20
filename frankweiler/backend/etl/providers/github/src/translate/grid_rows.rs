//! Build the `grid_rows` sidecar for one GitHub PR document.
//!
//! Under the single-doc-per-PR model, every row in a PR's sidecar shares
//! the same `qmd_path` (the PR's `index.md`). Each individual comment is
//! still its own row keyed by its provider-namespaced UUID — clicks in
//! the grid scroll to `data-msg-index="N"` inside the unified doc, where
//! `N` is the row's `message_index`.
//!
//! Row order (and therefore `message_index`) matches the rendered doc:
//! reviews → general → inline (grouped by `(path, line)` lex, then
//! chronological within each thread).

use std::collections::{BTreeMap, BTreeSet};
use std::hash::{Hash, Hasher};

use frankweiler_schema::grid_rows::GridRow;
use serde_json::Value;

use super::parse::{CommentRow, CommentSection, PullRequestRow};

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

fn comment_json(c: &CommentRow) -> Value {
    serde_json::json!({
        "uuid": c.uuid,
        "kind": c.kind,
        "section": format!("{:?}", c.section),
        "external_id": c.external_id,
        "in_reply_to_id": c.in_reply_to_id,
        "user_login": c.user_login,
        "body": c.body,
        "path": c.path,
        "line": c.line,
        "commit_id": c.commit_id,
        "state": c.state,
        "created_at": c.created_at,
        "updated_at": c.updated_at,
    })
}

fn pr_json(pr: &PullRequestRow) -> Value {
    serde_json::json!({
        "uuid": pr.uuid,
        "repo": pr.repo_full_name,
        "pr_number": pr.pr_number,
        "title": pr.title,
        "body": pr.body,
        "state": pr.state,
        "head_sha": pr.head_sha,
        "base_sha": pr.base_sha,
        "merged_at": pr.merged_at,
        "updated_at": pr.updated_at,
    })
}

pub fn fingerprint_for_pr(pr: &PullRequestRow, comments: &[CommentRow]) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    RENDER_VERSION.hash(&mut h);
    serde_json::to_string(&canonicalize(&pr_json(pr)))
        .unwrap_or_default()
        .hash(&mut h);
    // Sort comments deterministically (external_id is stable).
    let mut sorted: Vec<&CommentRow> = comments.iter().collect();
    sorted.sort_by_key(|c| c.external_id);
    for c in sorted {
        serde_json::to_string(&canonicalize(&comment_json(c)))
            .unwrap_or_default()
            .hash(&mut h);
    }
    format!("{:016x}", h.finish())
}

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

pub fn rows_for_pr(pr: &PullRequestRow, comments: &[CommentRow]) -> Vec<GridRow> {
    let qmd = super::render::pr_qmd_path_rel(&pr.repo_full_name, pr.pr_number);
    let entire_chat = format!("/chat/{}", pr.uuid);

    let mut rows: Vec<GridRow> = Vec::new();
    rows.push(GridRow {
        uuid: pr.uuid.clone(),
        provider: "github".into(),
        kind: "GitHub PR".into(),
        source_label: "GitHub".into(),
        when_ts: pr
            .updated_at
            .clone()
            .or_else(|| pr.created_at.clone())
            .unwrap_or_default(),
        author: pr.user_login.clone(),
        account: None,
        project: Some(pr.repo_full_name.clone()),
        channel: None,
        conversation_name: Some(pr.title.clone()),
        conversation_uuid: pr.uuid.clone(),
        message_index: None,
        entire_chat: entire_chat.clone(),
        text: if pr.body.is_empty() {
            pr.title.clone()
        } else {
            format!("{}\n\n{}", pr.title, pr.body)
        },
        slack_link: None,
        qmd_path: Some(qmd.clone()),
        source_url: pr.html_url.clone(),
        git_sha: pr.head_sha.clone(),
        external_id: Some(pr.pr_number.to_string()),
        notion_page_uuid: None,
        notion_block_uuid: None,
        document_uuid: Some(pr.uuid.clone()),
    });

    for (idx, c) in ordered_comments(comments).into_iter().enumerate() {
        rows.push(GridRow {
            uuid: c.uuid.clone(),
            provider: "github".into(),
            kind: c.kind.into(),
            source_label: "GitHub".into(),
            when_ts: c.created_at.clone(),
            author: c.user_login.clone(),
            account: None,
            project: Some(pr.repo_full_name.clone()),
            channel: None,
            conversation_name: Some(pr.title.clone()),
            conversation_uuid: pr.uuid.clone(),
            message_index: Some(idx as i64),
            entire_chat: entire_chat.clone(),
            text: c.body.clone(),
            slack_link: None,
            qmd_path: Some(qmd.clone()),
            source_url: c.html_url.clone(),
            git_sha: c.commit_id.clone(),
            external_id: Some(c.external_id.to_string()),
            notion_page_uuid: None,
            notion_block_uuid: None,
            document_uuid: Some(pr.uuid.clone()),
        });
    }
    rows
}
