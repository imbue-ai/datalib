//! Anthropic (Claude) render: convert parsed conversations into the
//! shared `chat-common` normalized model and delegate markdown /
//! grid-row / sidecar plumbing to
//! [`frankweiler_etl_chat_common::render::render_all`].
//!
//! One conversation → one [`NormalizedChat`] (single `"all"` bucket);
//! `chat_uuid`/`markdown_uuid` are the upstream `conversation_uuid`, so
//! page identities / links stay stable. The page title links out to
//! `claude.ai/chat/<uuid>`, and `org_uuid`/`org_name` ride along on
//! every grid row.
//!
//! Each Claude message is *exploded* into one [`NormalizedChatItem`] for
//! its text (+ extracted-text attachments + downloadable files) plus one
//! item per `thinking` / `tool_use` / `tool_result` block. The block
//! items keep their stable `tu-`/`tr-`/`th-` ids and the role-/block-
//! distinguished `kind_label` ("LLM Thinking" / "Tool Call"), so the
//! per-block grid rows the UI links to are preserved.
//!
//! Incrementality is unchanged and still dolt-diff driven: `parse`
//! narrowed to changed conversations, so we pass an empty
//! `prior_fingerprints` map and advance the cursor on success.

use std::collections::HashMap;

use anyhow::{Context as _, Result};
use serde_json::Value;

use frankweiler_etl::blob_cas::BlobBundle;
use frankweiler_etl::grid_index::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl::render_cursor;
use frankweiler_etl_chat_common::render::{render_all as cc_render_all, RenderProfile};
use frankweiler_etl_chat_common::types::{
    ItemKind, NormalizedAttachment, NormalizedChat, NormalizedChatItem, NormalizedDoc,
};

use super::parse::{
    shred, AttachmentRow, ContentBlockRow, MessageRow, ParsedExport, ShreddedConversation,
};

/// Bump when the item-shape / column mapping changes meaningfully.
/// v3: render via chat-common (block-explosion).
pub const RENDER_VERSION: u32 = 3;

fn profile() -> RenderProfile {
    RenderProfile {
        provider: "anthropic",
        source_label: "Claude".to_string(),
        chat_kind: "Chat".to_string(),
        // Per-item kind is always set via `kind_label`; nominal fallback.
        message_kind: "LLM Response".to_string(),
        reaction_kind: "Claude Reaction".to_string(),
        render_version: RENDER_VERSION,
    }
}

pub fn render_all(
    parsed: &ParsedExport,
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
        "[render] anthropic dolt_diff scan"
    );

    let mut chats: Vec<NormalizedChat> = Vec::with_capacity(parsed.conversations.len());
    let mut blobs_by_chat: HashMap<String, BlobBundle> = HashMap::new();
    for c in &parsed.conversations {
        let shredded = shred(c);
        let chat = build_chat(&shredded);
        blobs_by_chat.insert(chat.id.clone(), c.blobs.clone());
        chats.push(chat);
    }

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
    .context("anthropic chat-common render")?;

    if let Some(head) = parsed.scan.new_head.as_deref() {
        let cursor_path = render_cursor::cursor_path(root, source_name);
        render_cursor::write(&cursor_path, head, parsed.scan.scan_elapsed)
            .with_context(|| format!("write anthropic render cursor {}", cursor_path.display()))?;
    }
    Ok(())
}

/// One [`NormalizedChat`] per conversation, messages exploded into items.
fn build_chat(shredded: &ShreddedConversation) -> NormalizedChat {
    let conv = &shredded.conv;
    let conv_uuid = conv.conversation_uuid.clone();
    let model = conv
        .raw_json
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let mut blocks_by_msg: HashMap<&str, Vec<&ContentBlockRow>> = HashMap::new();
    for b in &shredded.content_blocks {
        blocks_by_msg.entry(&b.message_uuid).or_default().push(b);
    }
    let mut atts_by_msg: HashMap<&str, Vec<&AttachmentRow>> = HashMap::new();
    for a in &shredded.attachments {
        atts_by_msg.entry(&a.message_uuid).or_default().push(a);
    }
    let mut msgs: Vec<&MessageRow> = shredded.messages.iter().collect();
    msgs.sort_by(|a, b| {
        (
            a.created_at.as_deref().unwrap_or(""),
            a.message_uuid.as_str(),
        )
            .cmp(&(
                b.created_at.as_deref().unwrap_or(""),
                b.message_uuid.as_str(),
            ))
    });

    let mut items: Vec<NormalizedChatItem> = Vec::new();
    let mut last_ms = conv.created_at.as_deref().and_then(iso_to_ms);
    for m in &msgs {
        let msg_ms = m
            .created_at
            .as_deref()
            .and_then(iso_to_ms)
            .or_else(|| last_ms.map(|p| p + 1))
            .unwrap_or(0);
        last_ms = Some(msg_ms);

        let sender = m.sender.as_deref().unwrap_or("unknown");
        let kind_label = kind_for_sender(sender);
        let author_display = match kind_label {
            "LLM Response" => filter_nonempty(model.clone()).unwrap_or_else(|| "Assistant".into()),
            _ => capitalize(sender),
        };

        let mut blocks = blocks_by_msg
            .get(m.message_uuid.as_str())
            .cloned()
            .unwrap_or_default();
        blocks.sort_by_key(|b| b.block_index);

        // The message item: its `text` blocks, plus any extracted-text
        // attachments folded inline and downloadable files as
        // attachments. Always emitted so the per-message grid row stays.
        let mut body_parts: Vec<String> = blocks
            .iter()
            .filter(|b| b.r#type.as_deref() == Some("text"))
            .filter_map(|b| b.text.as_deref())
            .filter(|s| !s.is_empty())
            .map(|s| s.trim_end().to_string())
            .collect();

        let mut atts = atts_by_msg
            .get(m.message_uuid.as_str())
            .cloned()
            .unwrap_or_default();
        atts.sort_by_key(|a| a.attachment_index);
        let mut norm_atts: Vec<NormalizedAttachment> = Vec::new();
        for at in &atts {
            let (id, name, is_image) = attachment_meta(at);
            if at.kind == "attachment" {
                // Extracted text (no bytes) → folded into the body.
                let extracted = at
                    .raw_json
                    .as_object()
                    .and_then(|o| o.get("extracted_content"))
                    .and_then(Value::as_str);
                body_parts.push(render_extracted_attachment(
                    name.unwrap_or("(unnamed)"),
                    extracted,
                ));
            } else if let Some(id) = id {
                // Downloadable file → chat-common materializes via ref_id.
                norm_atts.push(NormalizedAttachment {
                    rel_path: None,
                    file_name: name.map(str::to_string),
                    mime_type: is_image.then(|| "image/png".to_string()),
                    byte_len: None,
                    source_url: None,
                    ref_id: Some(id.to_string()),
                });
            }
        }

        // One item per structural block (thinking / tool_use /
        // tool_result), keeping its stable section id + block kind.
        // Emitted before the message's own text item so that on a
        // timestamp tie the blocks (which precede the final answer) sort
        // first under the stable sort below.
        for b in &blocks {
            let btype = b.r#type.as_deref().unwrap_or("");
            if !matches!(btype, "tool_use" | "tool_result" | "thinking") {
                continue;
            }
            let raw_obj = b.raw_json.as_object().cloned().unwrap_or_default();
            let section_uuid =
                section_uuid_for_block(&m.message_uuid, b.block_index, Some(btype), &raw_obj)
                    .unwrap_or_else(|| format!("blk-{}-{}", m.message_uuid, b.block_index));
            let block_ms = b
                .start_timestamp
                .as_deref()
                .filter(|s| !s.is_empty())
                .and_then(iso_to_ms)
                .unwrap_or_else(|| msg_ms + (b.block_index as i64) + 1);
            let block_author = filter_nonempty(model.clone()).unwrap_or_else(|| btype.to_string());
            let body = block_body_md(btype, b.text.as_deref(), &raw_obj);
            items.push(NormalizedChatItem {
                message_uuid: section_uuid,
                author_id: btype.to_string(),
                author_display: block_author,
                date_ms: block_ms,
                text: filter_nonempty(body),
                kind: ItemKind::Text,
                attachments: Vec::new(),
                reactions: Vec::new(),
                system_note: None,
                source_url: None,
                kind_label: Some(kind_for_block(btype).to_string()),
            });
        }

        // The message's own item: its text blocks + extracted-text
        // attachments + downloadable files. Always emitted (even empty)
        // so the per-message grid row survives.
        let body = body_parts.join("\n\n");
        let kind = if norm_atts.is_empty() {
            ItemKind::Text
        } else {
            ItemKind::Attachment
        };
        items.push(NormalizedChatItem {
            message_uuid: m.message_uuid.clone(),
            author_id: sender.to_string(),
            author_display: author_display.clone(),
            date_ms: msg_ms,
            text: filter_nonempty(body),
            kind,
            attachments: norm_atts,
            reactions: Vec::new(),
            system_note: None,
            source_url: None,
            kind_label: Some(kind_label.to_string()),
        });
    }

    // Stable-sort items chronologically: blocks (earlier timestamps,
    // emitted first) fall before the message's final text on a tie, so a
    // turn reads thinking → tool calls → answer.
    items.sort_by_key(|i| i.date_ms);

    let title = conv
        .name
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(untitled)".to_string());
    NormalizedChat {
        id: conv_uuid.clone(),
        chat_uuid: conv_uuid.clone(),
        display: title.clone(),
        title: Some(title),
        account: Some(conv.account_uuid.clone()),
        project: conv.project_uuid.clone(),
        external_id: None,
        source_url: Some(format!("https://claude.ai/chat/{conv_uuid}")),
        org_uuid: conv.org_uuid.clone(),
        org_name: conv.org_name.clone(),
        buckets: vec![NormalizedDoc {
            period_key: "all".to_string(),
            markdown_uuid: conv_uuid,
            items,
        }],
    }
}

fn kind_for_sender(sender: &str) -> &'static str {
    match sender.to_ascii_lowercase().as_str() {
        "human" | "user" => "User Input",
        "assistant" => "LLM Response",
        _ => "Tool Call",
    }
}

fn kind_for_block(block_type: &str) -> &'static str {
    if block_type == "thinking" {
        "LLM Thinking"
    } else {
        "Tool Call"
    }
}

fn filter_nonempty(s: String) -> Option<String> {
    (!s.trim().is_empty()).then_some(s)
}

/// Parse an ISO-8601 timestamp to unix millis; `None` on anything
/// unparseable (callers fall back to a bumped previous time).
fn iso_to_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

// ─────────────────────────────────────────────────────────────────────
// Block / attachment rendering (the markdown that becomes item.text).
// ─────────────────────────────────────────────────────────────────────

/// The grid-row `uuid` (and the item's `message_uuid`) for a structural
/// block. `tu-` for `tool_use`, `tr-` for the matching `tool_result`,
/// `th-` for `thinking` (synthesized from `{msg_uuid}-{block_index}`).
/// `None` for plain `text`, which lives in the parent message item.
pub(crate) fn section_uuid_for_block(
    msg_uuid: &str,
    block_index: usize,
    btype: Option<&str>,
    raw_obj: &serde_json::Map<String, Value>,
) -> Option<String> {
    match btype {
        Some("tool_use") => raw_obj
            .get("id")
            .and_then(Value::as_str)
            .map(|id| format!("tu-{id}")),
        Some("tool_result") => raw_obj
            .get("tool_use_id")
            .and_then(Value::as_str)
            .map(|id| format!("tr-{id}")),
        Some("thinking") => Some(format!("th-{msg_uuid}-{block_index}")),
        _ => None,
    }
}

/// Render one `thinking` / `tool_use` / `tool_result` block to the
/// markdown body of its own item (the `<details>` block the UI shows).
fn block_body_md(
    btype: &str,
    btext: Option<&str>,
    raw_obj: &serde_json::Map<String, Value>,
) -> String {
    let lines: Vec<String> = match btype {
        "thinking" => {
            let thought = raw_obj
                .get("thinking")
                .and_then(Value::as_str)
                .or(btext)
                .unwrap_or("");
            if thought.is_empty() {
                vec![]
            } else {
                let quoted = format!("> {}", thought.trim_end().replace('\n', "\n> "));
                vec![
                    "<details><summary>Thinking</summary>".into(),
                    String::new(),
                    quoted,
                    String::new(),
                    "</details>".into(),
                ]
            }
        }
        "tool_use" => {
            let name = raw_obj
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let msg = raw_obj.get("message").and_then(Value::as_str);
            let summary = match msg {
                Some(m) => format!("Tool use: {name} — {m}"),
                None => format!("Tool use: {name}"),
            };
            let mut out = vec![
                format!("<details><summary>{summary}</summary>"),
                String::new(),
            ];
            if let Some(tool_input) = raw_obj.get("input") {
                if !json_is_empty(tool_input) {
                    out.push("```json".into());
                    out.push(json_pretty_sorted(tool_input));
                    out.push("```".into());
                }
            }
            out.push("</details>".into());
            out
        }
        "tool_result" => {
            let name = raw_obj
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let is_err = raw_obj
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let summary = if is_err {
                format!("Tool result: {name} (error)")
            } else {
                format!("Tool result: {name}")
            };
            let mut out = vec![
                format!("<details><summary>{summary}</summary>"),
                String::new(),
            ];
            render_tool_result_content(raw_obj.get("content"), &mut out);
            out.push("</details>".into());
            out
        }
        _ => btext
            .filter(|t| !t.is_empty())
            .map(|t| vec![t.trim_end().to_string()])
            .unwrap_or_default(),
    };
    lines.join("\n")
}

fn render_tool_result_content(content: Option<&Value>, out: &mut Vec<String>) {
    match content {
        Some(Value::String(s)) => {
            out.push("```".into());
            out.push(s.trim_end().into());
            out.push("```".into());
        }
        Some(Value::Array(items)) => {
            for item in items {
                match item {
                    Value::Object(m)
                        if m.get("type").and_then(Value::as_str) == Some("text")
                            && m.get("text")
                                .and_then(Value::as_str)
                                .is_some_and(|t| !t.is_empty()) =>
                    {
                        out.push(
                            m.get("text")
                                .and_then(Value::as_str)
                                .unwrap()
                                .trim_end()
                                .into(),
                        );
                        out.push(String::new());
                    }
                    Value::Object(_) => {
                        out.push("```json".into());
                        out.push(json_pretty_sorted(item));
                        out.push("```".into());
                        out.push(String::new());
                    }
                    other => {
                        out.push("```".into());
                        out.push(
                            match other {
                                Value::String(s) => s.clone(),
                                v => v.to_string(),
                            }
                            .trim_end()
                            .into(),
                        );
                        out.push("```".into());
                        out.push(String::new());
                    }
                }
            }
        }
        Some(v) if !v.is_null() => {
            out.push("```json".into());
            out.push(json_pretty_sorted(v));
            out.push("```".into());
        }
        _ => {}
    }
}

/// Falsy-ish check mirroring Python `if tool_input:` — skip empty
/// object/array/string/zero.
fn json_is_empty(v: &Value) -> bool {
    match v {
        Value::Object(m) => m.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::String(s) => s.is_empty(),
        Value::Bool(false) | Value::Null => true,
        Value::Number(n) => n.as_f64() == Some(0.0),
        _ => false,
    }
}

/// JSON dumped with `indent=2, sort_keys=true` (recursive key sort).
fn json_pretty_sorted(v: &Value) -> String {
    serde_json::to_string_pretty(&canonicalize(v)).unwrap_or_default()
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

/// Pull (file id, file name, is_image) out of an attachment row's
/// raw_json. Anthropic uses `file_uuid` / `id` / `uuid` for the id
/// depending on export vs live API.
fn attachment_meta(at: &AttachmentRow) -> (Option<&str>, Option<&str>, bool) {
    let raw_obj = at.raw_json.as_object();
    let id = raw_obj
        .and_then(|o| {
            o.get("file_uuid")
                .or_else(|| o.get("id"))
                .or_else(|| o.get("uuid"))
        })
        .and_then(Value::as_str);
    let name = raw_obj
        .and_then(|o| o.get("file_name").or_else(|| o.get("name")))
        .and_then(Value::as_str);
    let is_image = raw_obj
        .and_then(|o| o.get("file_kind").or_else(|| o.get("file_type")))
        .and_then(Value::as_str)
        .map(|s| s.eq_ignore_ascii_case("image") || s.starts_with("image/"))
        .unwrap_or(false);
    (id, name, is_image)
}

/// Render a Claude `attachments[]` text item inline (extracted upload
/// text; the binary is not retained).
fn render_extracted_attachment(label: &str, extracted: Option<&str>) -> String {
    let header_label = if label.is_empty() { "(unnamed)" } else { label };
    let body = extracted.unwrap_or("").trim();
    if body.is_empty() {
        return format!("**[attachment: {header_label}]** *(no extracted content)*");
    }
    let quoted: String = body.lines().map(|l| format!("> {l}\n")).collect();
    format!("**[attachment: {header_label}]**\n{quoted}")
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
