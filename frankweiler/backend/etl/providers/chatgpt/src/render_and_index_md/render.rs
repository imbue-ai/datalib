//! ChatGPT render: convert parsed conversations into the shared
//! `chat-common` normalized model and delegate markdown / grid-row /
//! sidecar plumbing to [`frankweiler_etl_chat_common::render::render_all`].
//!
//! One conversation → one [`NormalizedChat`] with a single `"all"`
//! bucket; `chat_uuid` and the bucket's `markdown_uuid` are the upstream
//! `conversation_id`, so page identities / links stay stable. Each
//! message becomes one [`NormalizedChatItem`] whose `kind_label`
//! carries the role-distinguished grid kind ("User Input" / "LLM
//! Response" / "LLM Thinking" / "Tool Call"). The page title links out
//! to `chatgpt.com/c/<id>`.
//!
//! Incrementality is unchanged and still dolt-diff driven: `parse`
//! consulted `dolt_diff_<table>` against the render cursor and only
//! loaded changed conversations, so we pass an empty `prior_fingerprints`
//! map and advance the cursor on success.

use std::collections::{HashMap, HashSet};

use anyhow::{Context as _, Result};
use frankweiler_etl::blob_cas::BlobBundle;
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl::render_cursor;
use frankweiler_etl_chat_common::render::{render_all as cc_render_all, RenderProfile};
use frankweiler_etl_chat_common::types::{
    ItemKind, NormalizedAttachment, NormalizedChat, NormalizedChatItem, NormalizedDoc,
};

use super::parse::{
    shred, OAAttachmentRef, OAContentPartRow, OAMessageRow, ParsedChatGPTApi, ShreddedConversation,
};

/// Bump when the item-shape / column mapping changes meaningfully.
/// v4: render via chat-common.
pub const RENDER_VERSION: u32 = 4;

fn profile() -> RenderProfile {
    RenderProfile {
        provider: "openai",
        source_label: "ChatGPT".to_string(),
        chat_kind: "Chat".to_string(),
        // Per-message kind is always set via `kind_label`; this is only a
        // nominal fallback.
        message_kind: "LLM Response".to_string(),
        reaction_kind: "ChatGPT Reaction".to_string(),
        render_version: RENDER_VERSION,
    }
}

/// Render every conversation in `parsed` via the shared chat renderer.
pub fn render_all(
    parsed: &ParsedChatGPTApi,
    root: &std::path::Path,
    source_name: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<()> {
    let elapsed_ms = parsed.scan.scan_elapsed.map(|d| d.as_millis() as u64);
    tracing::info!(
        source = source_name,
        scan_elapsed_ms = elapsed_ms,
        changed_conversations = parsed
            .scan
            .changed_conversations
            .as_ref()
            .map(|s| s.len() as i64)
            .unwrap_or(-1),
        cold_start = parsed.scan.changed_conversations.is_none(),
        "[translate] chatgpt dolt_diff scan"
    );

    let mut chats: Vec<NormalizedChat> = Vec::with_capacity(parsed.conversations.len());
    let mut blobs_by_chat: HashMap<String, BlobBundle> = HashMap::new();
    for c in &parsed.conversations {
        let shredded = shred(c);
        let chat = build_chat(&shredded);
        blobs_by_chat.insert(chat.id.clone(), c.blobs.clone());
        chats.push(chat);
    }

    // Skip is driven upstream by dolt_diff; the fingerprint map is empty.
    let no_priors: HashMap<String, String> = HashMap::new();
    cc_render_all(
        &profile(),
        &chats,
        root,
        source_name,
        &blobs_by_chat,
        progress,
        &no_priors,
        on_doc_complete,
    )
    .context("chatgpt chat-common render")?;

    if let Some(head) = parsed.scan.new_head.as_deref() {
        let cursor_path = render_cursor::cursor_path(root, source_name);
        render_cursor::write(&cursor_path, head, parsed.scan.scan_elapsed)
            .with_context(|| format!("write chatgpt render cursor {}", cursor_path.display()))?;
    }
    Ok(())
}

/// One [`NormalizedChat`] per conversation. Messages are ordered by the
/// `current_node → root` parent walk (falling back to a `create_time`
/// sort), one [`NormalizedChatItem`] each.
fn build_chat(shredded: &ShreddedConversation) -> NormalizedChat {
    let conv = &shredded.conv;
    let conv_id = conv.conversation_id.clone();

    let mut parts_by_msg: HashMap<&str, Vec<&OAContentPartRow>> = HashMap::new();
    for p in &shredded.content_parts {
        parts_by_msg
            .entry(p.message_id.as_str())
            .or_default()
            .push(p);
    }

    let path = ordered_messages(shredded);
    let mut items: Vec<NormalizedChatItem> = Vec::with_capacity(path.len());
    // Mirror the renderer's timestamp bump: a message with no create_time
    // inherits the previous item's time + 1ms so ordering stays stable.
    let mut last_ms = conv.create_time.as_deref().and_then(iso_to_ms);
    for m in &path {
        let ms = m
            .create_time
            .as_deref()
            .and_then(iso_to_ms)
            .or_else(|| last_ms.map(|p| p + 1))
            .unwrap_or(0);
        last_ms = Some(ms);

        let kind_label = kind_for_role_and_type(m.role.as_deref(), m.content_type.as_deref());
        let author_display = match kind_label {
            "User Input" => "User".to_string(),
            "LLM Response" | "LLM Thinking" => m
                .model_slug
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| "Assistant".to_string()),
            _ => capitalize(m.role.as_deref().unwrap_or("unknown")),
        };

        let mut parts = parts_by_msg
            .get(m.message_id.as_str())
            .cloned()
            .unwrap_or_default();
        parts.sort_by_key(|p| p.part_index);
        let body = render_message_body(&parts);

        let attachments: Vec<NormalizedAttachment> =
            m.attachments.iter().map(att_to_norm).collect();
        let kind = if attachments.is_empty() {
            ItemKind::Text
        } else {
            ItemKind::Attachment
        };

        items.push(NormalizedChatItem {
            message_uuid: m.message_id.clone(),
            author_id: m.role.clone().unwrap_or_else(|| "unknown".into()),
            author_display,
            date_ms: ms,
            text: body,
            kind,
            attachments,
            reactions: Vec::new(),
            system_note: None,
            source_url: None,
            kind_label: Some(kind_label.to_string()),
        });
    }

    let title = conv
        .title
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(untitled)".to_string());
    NormalizedChat {
        id: conv_id.clone(),
        chat_uuid: conv_id.clone(),
        display: title.clone(),
        title: Some(title),
        account: conv.account_id.clone(),
        project: None,
        external_id: None,
        source_url: Some(format!("https://chatgpt.com/c/{conv_id}")),
        org_uuid: None,
        org_name: None,
        buckets: vec![NormalizedDoc {
            period_key: "all".to_string(),
            markdown_uuid: conv_id,
            items,
        }],
    }
}

/// Walk `current_node → root` via `parent_id`; fall back to a
/// `create_time` sort when the tree is missing/broken.
fn ordered_messages(shredded: &ShreddedConversation) -> Vec<&OAMessageRow> {
    let msg_by_id: HashMap<&str, &OAMessageRow> = shredded
        .messages
        .iter()
        .map(|m| (m.message_id.as_str(), m))
        .collect();
    let mut path: Vec<&OAMessageRow> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut cursor = shredded.conv.current_node.clone();
    while let Some(cid) = cursor {
        if !seen.insert(cid.clone()) {
            break;
        }
        let Some(m) = msg_by_id.get(cid.as_str()) else {
            break;
        };
        path.push(*m);
        cursor = m.parent_id.clone();
    }
    path.reverse();
    if path.is_empty() {
        let mut sorted: Vec<&OAMessageRow> = shredded.messages.iter().collect();
        sorted.sort_by(|a, b| {
            a.create_time
                .as_deref()
                .unwrap_or("")
                .cmp(b.create_time.as_deref().unwrap_or(""))
        });
        path = sorted;
    }
    path
}

/// Render a message's content parts into one markdown body: plain text,
/// fenced code (with language), fenced execution output, and reasoning
/// as a blockquote. Returns `None` when there's nothing to show.
fn render_message_body(parts: &[&OAContentPartRow]) -> Option<String> {
    let mut blocks: Vec<String> = Vec::new();
    for p in parts {
        let has_text = p.text.as_deref().is_some_and(|s| !s.is_empty());
        if !has_text && p.kind != "execution_output" && p.kind != "code" {
            continue;
        }
        let t = p.text.as_deref().unwrap_or("").trim_end();
        match p.kind.as_str() {
            "text" => blocks.push(t.to_string()),
            "code" => blocks.push(format!(
                "```{}\n{t}\n```",
                p.language.as_deref().unwrap_or("")
            )),
            "execution_output" => blocks.push(format!("```\n{t}\n```")),
            "thoughts" | "reasoning_recap" => blocks.push(format!("> {}", t.replace('\n', "\n> "))),
            _ => blocks.push(t.to_string()),
        }
    }
    let body = blocks.join("\n\n");
    (!body.trim().is_empty()).then_some(body)
}

/// ChatGPT file ref → normalized attachment. The bytes resolve via
/// `ref_id` (the `file_id`) against the conversation's bundle.
fn att_to_norm(a: &OAAttachmentRef) -> NormalizedAttachment {
    NormalizedAttachment {
        rel_path: None,
        file_name: a.name.clone(),
        // chat-common only checks the `image/` prefix to pick inline
        // rendering; the exact subtype is immaterial.
        mime_type: a.is_image.then(|| "image/png".to_string()),
        byte_len: None,
        source_url: None,
        ref_id: Some(a.file_id.clone()),
    }
}

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

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => {
            let mut out: String = c.to_uppercase().collect();
            for rest in chars {
                out.extend(rest.to_lowercase());
            }
            out
        }
    }
}

/// Parse an ISO-8601 timestamp to unix millis. Accepts `…Z` and explicit
/// offsets; returns `None` on anything unparseable (callers fall back to
/// the bumped previous time).
fn iso_to_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis())
}
