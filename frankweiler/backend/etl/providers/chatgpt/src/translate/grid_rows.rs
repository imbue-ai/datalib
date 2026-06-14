//! Port of the `_openai_*` helpers + per-conversation row emit loop from
//! `src/ingest/grid_rows.py`. Produces one `GridRow` per ChatGPT
//! conversation (Chat row) plus one row per surfaced message; the
//! translator pairs these with the rendered `.md` in a
//! `.grid_rows.json` sidecar consumed by the provider-agnostic Load
//! step.

use frankweiler_schema::grid_rows::GridRow;

use super::parse::{OAConversationRow, OAMessageRow, ShreddedConversation};

/// Bumped on render-layout changes so a forced rebake is possible even
/// when the upstream payload hasn't moved. Matches the constant on the
/// Slack side.
///
/// Bumped from 1 to 2 when `data-msg-index` was dropped in favor of
/// `data-section-uuid` on the per-message wrapper div.
///
/// Bumped from 2 to 3 when assistant text started getting
/// sentinel-cleaned (U+E200/E201/E202 wrappers stripped or rewritten
/// into markdown links) and attachment link targets started
/// percent-encoding spaces/parens so images with spaces in their
/// names actually resolve.
pub const RENDER_VERSION: u32 = 3;

fn kind_for_role_and_type(role: Option<&str>, content_type: Option<&str>) -> &'static str {
    match role.unwrap_or("").to_ascii_lowercase().as_str() {
        "user" => "User Input",
        "assistant" => match content_type {
            Some("thoughts") | Some("reasoning_recap") => "LLM Thinking",
            _ => "LLM Response",
        },
        _ => "Tool Call",
    }
}

fn bump_micros(ts: &str, n: i64) -> String {
    frankweiler_time::bump_micros_str(ts, n).unwrap_or_else(|| ts.into())
}

fn qmd_path(account_id: Option<&str>, conv_id: &str) -> String {
    // Page-dir layout: `<conv_id>/index.md`. Matches
    // `Rendered::relative_path` in `render.rs`.
    format!(
        "rendered_md/openai/{}/llm_chats/{conv_id}/index.md",
        account_id.unwrap_or("unknown"),
    )
}

/// Build all grid rows for a single conversation, in stable order
/// (Chat row first, then messages by `(create_time, message_id)`).
pub fn rows_for_conversation(shredded: &ShreddedConversation) -> Vec<GridRow> {
    let conv = &shredded.conv;
    let mut rows = Vec::new();
    rows.push(chat_row(conv));

    let mut msgs: Vec<&OAMessageRow> = shredded.messages.iter().collect();
    msgs.sort_by(|a, b| {
        let ka = (
            a.create_time.as_deref().unwrap_or(""),
            a.message_id.as_str(),
        );
        let kb = (
            b.create_time.as_deref().unwrap_or(""),
            b.message_id.as_str(),
        );
        ka.cmp(&kb)
    });

    let conv_time = conv
        .create_time
        .clone()
        .or_else(|| conv.update_time.clone());

    for (idx, m) in msgs.iter().enumerate() {
        rows.push(message_row(conv, m, idx, conv_time.as_deref()));
    }
    rows
}

fn chat_row(conv: &OAConversationRow) -> GridRow {
    let when = conv
        .create_time
        .clone()
        .or_else(|| conv.update_time.clone());
    GridRow {
        uuid: conv.conversation_id.clone(),
        provider: "openai".into(),
        kind: "Chat".into(),
        source_label: "ChatGPT".into(),
        when_ts: when,
        author: None,
        account: conv.account_id.clone(),
        org_uuid: None,
        org_name: None,
        project: None,
        channel: None,
        conversation_name: conv.title.clone(),
        conversation_uuid: conv.conversation_id.clone(),
        message_index: None,
        entire_chat: format!("/chat/{}", conv.conversation_id),
        text: conv.title.clone().unwrap_or_default(),
        slack_link: None,
        qmd_path: Some(qmd_path(conv.account_id.as_deref(), &conv.conversation_id)),
        source_url: None,
        git_sha: None,
        external_id: None,
        notion_page_uuid: None,
        notion_block_uuid: None,
        markdown_uuid: Some(conv.conversation_id.clone()),
    }
}

fn message_row(
    conv: &OAConversationRow,
    m: &OAMessageRow,
    idx: usize,
    conv_time: Option<&str>,
) -> GridRow {
    let kind = kind_for_role_and_type(m.role.as_deref(), m.content_type.as_deref());
    let author = match kind {
        "User Input" => conv.account_id.clone(),
        "LLM Response" | "LLM Thinking" => m.model_slug.clone().or_else(|| m.role.clone()),
        _ => m.role.clone(),
    };
    let when: Option<String> = m
        .create_time
        .clone()
        .or_else(|| conv_time.map(|t| bump_micros(t, (idx + 1) as i64)));
    GridRow {
        uuid: m.message_id.clone(),
        provider: "openai".into(),
        kind: kind.into(),
        source_label: "ChatGPT".into(),
        when_ts: when,
        author,
        account: conv.account_id.clone(),
        org_uuid: None,
        org_name: None,
        project: None,
        channel: None,
        conversation_name: conv.title.clone(),
        conversation_uuid: conv.conversation_id.clone(),
        message_index: Some(idx as i64),
        entire_chat: format!("/chat/{}", conv.conversation_id),
        text: m.text.clone(),
        slack_link: None,
        qmd_path: Some(qmd_path(conv.account_id.as_deref(), &conv.conversation_id)),
        source_url: None,
        git_sha: None,
        external_id: None,
        notion_page_uuid: None,
        notion_block_uuid: None,
        markdown_uuid: Some(conv.conversation_id.clone()),
    }
}
