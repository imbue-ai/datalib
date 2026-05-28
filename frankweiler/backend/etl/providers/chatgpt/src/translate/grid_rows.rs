//! Port of the `_openai_*` helpers + per-conversation row emit loop from
//! `src/ingest/grid_rows.py`. Produces one `GridRow` per ChatGPT
//! conversation (Chat row) plus one row per surfaced message; the
//! translator pairs these with the rendered `.md` in a
//! `.grid_rows.json` sidecar consumed by the provider-agnostic Load
//! step.

use std::hash::{Hash, Hasher};

use chrono::{DateTime, FixedOffset};
use frankweiler_schema::grid_rows::GridRow;
use serde_json::Value;

use super::parse::{OAConversationRow, OAMessageRow, ParsedChatGPTApi};

/// Bumped on render-layout changes so a forced rebake is possible even
/// when the upstream payload hasn't moved. Matches the constant on the
/// Slack side.
///
/// Bumped from 1 to 2 when `data-msg-index` was dropped in favor of
/// `data-section-uuid` on the per-message wrapper div.
pub const RENDER_VERSION: u32 = 2;

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
    if ts.is_empty() {
        return ts.into();
    }
    let normalized = if let Some(prefix) = ts.strip_suffix('Z') {
        format!("{prefix}+00:00")
    } else {
        ts.to_string()
    };
    let Ok(dt) = DateTime::<FixedOffset>::parse_from_rfc3339(&normalized) else {
        return ts.into();
    };
    let bumped = dt + chrono::Duration::microseconds(n);
    bumped.format("%Y-%m-%dT%H:%M:%S%.6f%:z").to_string()
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
pub fn rows_for_conversation(parsed: &ParsedChatGPTApi, conversation_id: &str) -> Vec<GridRow> {
    let Some(conv) = parsed
        .conversations
        .iter()
        .find(|c| c.conversation_id == conversation_id)
    else {
        return Vec::new();
    };

    let mut rows = Vec::new();
    rows.push(chat_row(conv));

    let mut msgs: Vec<&OAMessageRow> = parsed
        .messages
        .iter()
        .filter(|m| m.conversation_id == conversation_id)
        .collect();
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
        .or_else(|| conv.update_time.clone())
        .unwrap_or_default();

    for (idx, m) in msgs.iter().enumerate() {
        rows.push(message_row(conv, m, idx, &conv_time));
    }
    rows
}

fn chat_row(conv: &OAConversationRow) -> GridRow {
    let when = conv
        .create_time
        .clone()
        .or_else(|| conv.update_time.clone())
        .unwrap_or_default();
    GridRow {
        uuid: conv.conversation_id.clone(),
        provider: "openai".into(),
        kind: "Chat".into(),
        source_label: "ChatGPT".into(),
        when_ts: when,
        author: None,
        account: conv.account_id.clone(),
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
        document_uuid: Some(conv.conversation_id.clone()),
    }
}

fn message_row(conv: &OAConversationRow, m: &OAMessageRow, idx: usize, conv_time: &str) -> GridRow {
    let kind = kind_for_role_and_type(m.role.as_deref(), m.content_type.as_deref());
    let author = match kind {
        "User Input" => conv.account_id.clone(),
        "LLM Response" | "LLM Thinking" => m.model_slug.clone().or_else(|| m.role.clone()),
        _ => m.role.clone(),
    };
    let when = m
        .create_time
        .clone()
        .unwrap_or_else(|| bump_micros(conv_time, (idx + 1) as i64));
    GridRow {
        uuid: m.message_id.clone(),
        provider: "openai".into(),
        kind: kind.into(),
        source_label: "ChatGPT".into(),
        when_ts: when,
        author,
        account: conv.account_id.clone(),
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
        document_uuid: Some(conv.conversation_id.clone()),
    }
}

/// Stable hash over the raw upstream payload that drives this
/// conversation's render: the conversation row, every message row, and
/// every content part. Canonicalized (sorted keys) so cosmetic JSON
/// reordering doesn't invalidate the fingerprint.
pub fn fingerprint_for_conversation(parsed: &ParsedChatGPTApi, conversation_id: &str) -> String {
    let Some(conv) = parsed
        .conversations
        .iter()
        .find(|c| c.conversation_id == conversation_id)
    else {
        return "0000000000000000".into();
    };

    let mut h = std::collections::hash_map::DefaultHasher::new();
    RENDER_VERSION.hash(&mut h);
    canonical_json(&conv.raw_json).hash(&mut h);

    let mut msgs: Vec<&OAMessageRow> = parsed
        .messages
        .iter()
        .filter(|m| m.conversation_id == conversation_id)
        .collect();
    msgs.sort_by(|a, b| a.message_id.cmp(&b.message_id));
    for m in &msgs {
        m.message_id.hash(&mut h);
        canonical_json(&m.raw_json).hash(&mut h);
    }

    let mut parts: Vec<&super::parse::OAContentPartRow> = parsed
        .content_parts
        .iter()
        .filter(|p| msgs.iter().any(|m| m.message_id == p.message_id))
        .collect();
    parts.sort_by(|a, b| {
        a.message_id
            .cmp(&b.message_id)
            .then(a.part_index.cmp(&b.part_index))
    });
    for p in parts {
        p.message_id.hash(&mut h);
        p.part_index.hash(&mut h);
        canonical_json(&p.raw_json).hash(&mut h);
    }

    format!("{:016x}", h.finish())
}

fn canonical_json(v: &Value) -> String {
    serde_json::to_string(&canonicalize(v)).unwrap_or_default()
}

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
