//! Anthropic (Claude) render: convert parsed conversations into the
//! shared `chat-common` normalized model and delegate markdown /
//! grid-row / sidecar plumbing to
//! [`datalib_etl_chat_common::render::render_all`].
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

use datalib_etl::blob_cas::BlobBundle;
use datalib_etl::grid_index::RenderedMarkdown;
use datalib_etl::progress::Progress;
use datalib_etl::render_cursor;
use datalib_etl_chat_common::render::{
    render_all as cc_render_all, RenderProfile, ENTITY_KIND_CONVERSATION,
};
use datalib_etl_chat_common::types::{
    ItemKind, NormalizedAttachment, NormalizedChat, NormalizedChatItem, NormalizedDoc, UpstreamRef,
};

use super::ids;
use super::parse::{
    shred, AttachmentRow, ContentBlockRow, MessageRow, ParsedExport, ProjectRow,
    ShreddedConversation,
};

/// Bump when the item-shape / column mapping changes meaningfully.
/// v3: render via chat-common (block-explosion).
/// v4: projects render as their own pages, and a conversation's
///     `project` grid column carries the project name, not its UUID.
pub const RENDER_VERSION: u32 = 4;

fn profile() -> RenderProfile {
    RenderProfile {
        provider: "anthropic",
        source_label: "Claude".to_string(),
        chat_kind: "Chat".to_string(),
        // Per-item kind is always set via `kind_label`; nominal fallback.
        message_kind: "LLM Response".to_string(),
        reaction_kind: "Claude Reaction".to_string(),
        chat_entity_kind: ENTITY_KIND_CONVERSATION,
        render_version: RENDER_VERSION,
    }
}

/// Projects are not chats, but they are *page-shaped* in exactly the
/// way chat-common already handles: a titled page whose body is a list
/// of anchored sections, each with its own grid row. Reusing the same
/// renderer gets the `id="m-{uuid}"` / `data-section-uuid` anchors, the
/// sidecar, and the fingerprint skip for free — see docs/dev/cards.md
/// for why those anchors are load-bearing.
fn project_profile() -> RenderProfile {
    RenderProfile {
        provider: "anthropic",
        source_label: "Claude".to_string(),
        chat_kind: "Project".to_string(),
        message_kind: "Project Knowledge".to_string(),
        // Projects have no reactions; chat-common needs the field set.
        reaction_kind: "Claude Reaction".to_string(),
        // A project page is not a conversation, and its id was minted
        // as `KIND_PROJECT`. Leaving the chat-common default here
        // stamped a `"conversation"` backpointer that regenerated a
        // different uuid.
        chat_entity_kind: ids::KIND_PROJECT,
        render_version: RENDER_VERSION,
    }
}

/// Render-time knobs. Separate from the config struct so the render
/// layer doesn't depend on the config crate.
#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    /// See [`datalib_etl_anthropic_config::AnthropicRenderConfig::max_project_doc_bytes`].
    pub max_project_doc_bytes: Option<usize>,
}

impl Default for RenderOptions {
    fn default() -> Self {
        Self {
            max_project_doc_bytes: Some(128 * 1024),
        }
    }
}

pub fn render_all(
    parsed: &ParsedExport,
    root: &std::path::Path,
    source_name: &str,
    options: RenderOptions,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<()> {
    let elapsed_ms = parsed.scan.scan_elapsed.map(|d| d.as_millis() as u64);
    tracing::info!(
        source = source_name,
        scan_elapsed_ms = elapsed_ms,
        changed_buckets = parsed
            .scan
            .changed_buckets
            .as_ref()
            .map(|s| s.len() as i64)
            .unwrap_or(-1),
        conversations = parsed.conversations.len(),
        projects = parsed.projects.len(),
        cold_start = parsed.scan.changed_buckets.is_none(),
        "[render] anthropic dolt_diff scan"
    );

    let mut chats: Vec<NormalizedChat> = Vec::with_capacity(parsed.conversations.len());
    let mut blobs_by_chat: HashMap<String, BlobBundle> = HashMap::new();
    for c in &parsed.conversations {
        let shredded = shred(c);
        let chat = build_chat(&shredded, &parsed.project_name_by_uuid);
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

    // Projects are a second pass with their own profile. They share the
    // page-path namespace with conversations (`rendered_md/<source>/
    // <uuid>/all.md`) and can't collide: a project UUID is never a
    // conversation UUID. No blobs — knowledge docs carry their text
    // inline.
    if !parsed.projects.is_empty() {
        let project_chats: Vec<NormalizedChat> = parsed
            .projects
            .iter()
            .map(|p| build_project_page(p, &options))
            .collect();
        let no_blobs: HashMap<String, BlobBundle> = HashMap::new();
        cc_render_all(
            &project_profile(),
            &project_chats,
            root,
            source_name,
            &no_blobs,
            progress,
            &no_priors,
            on_doc_complete,
        )
        .context("anthropic project render")?;
    }

    if let Some(head) = parsed.scan.new_head.as_deref() {
        let cursor_path = render_cursor::cursor_path(root, source_name);
        render_cursor::write(
            &cursor_path,
            head,
            parsed.scan.scan_elapsed,
            &render_cursor::no_params(),
        )
        .with_context(|| format!("write anthropic render cursor {}", cursor_path.display()))?;
    }
    Ok(())
}

/// One [`NormalizedChat`] per conversation, messages exploded into items.
///
/// `project_names` resolves the conversation's `project_uuid` to the
/// human name that goes in the `project` grid column. An unresolved
/// UUID falls back to the UUID itself — that is what a mirror with
/// `sync.projects = false` looks like, and a raw id beats a blank cell.
fn build_chat(
    shredded: &ShreddedConversation,
    project_names: &HashMap<String, String>,
) -> NormalizedChat {
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
            let block_id = block_identity(&m.message_uuid, b.block_index, btype, &raw_obj);
            let block_ms = b
                .start_timestamp
                .as_deref()
                .filter(|s| !s.is_empty())
                .and_then(iso_to_ms)
                .unwrap_or_else(|| msg_ms + (b.block_index as i64) + 1);
            let block_author = filter_nonempty(model.clone()).unwrap_or_else(|| btype.to_string());
            let body = block_body_md(btype, b.text.as_deref(), &raw_obj);
            items.push(NormalizedChatItem {
                message_uuid: block_id.uuid.clone(),
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
                source_ref: Some(UpstreamRef::new(
                    block_id.entity_kind,
                    block_id.natural_key.clone(),
                )),
            });
        }

        // The message's own item: its text blocks + extracted-text
        // attachments + downloadable files. Always emitted (even empty)
        // so the per-message grid row survives.
        let msg_id = ids::message(&m.message_uuid);
        let body = body_parts.join("\n\n");
        let kind = if norm_atts.is_empty() {
            ItemKind::Text
        } else {
            ItemKind::Attachment
        };
        items.push(NormalizedChatItem {
            message_uuid: msg_id.uuid.clone(),
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
            source_ref: Some(UpstreamRef::new(
                msg_id.entity_kind,
                msg_id.natural_key.clone(),
            )),
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
    let chat_uuid = ids::conversation(&conv_uuid).uuid;
    NormalizedChat {
        id: chat_uuid.clone(),
        chat_uuid: chat_uuid.clone(),
        display: title.clone(),
        title: Some(title),
        account: Some(conv.account_uuid.clone()),
        project: conv.project_uuid.as_ref().map(|uuid| {
            project_names
                .get(uuid)
                .cloned()
                .unwrap_or_else(|| uuid.clone())
        }),
        // Anthropic's own conversation UUID — the only remaining route
        // back to claude.ai now that `uuid` is a minted v5, and what
        // the grid's "Copy source ID(s)" action reads.
        external_id: Some(conv_uuid.clone()),
        source_url: Some(format!("https://claude.ai/chat/{conv_uuid}")),
        upstream_scope: None,
        org_uuid: conv.org_uuid.clone(),
        org_name: conv.org_name.clone(),
        buckets: vec![NormalizedDoc {
            period_key: "all".to_string(),
            markdown_uuid: chat_uuid,
            items,
        }],
    }
}

/// One page per Claude Project: its description, its custom
/// instructions, and one section per knowledge document.
///
/// The two synthesized sections and each knowledge document get their
/// own minted ids ([`super::ids`]), keyed on the project UUID and the
/// document UUID respectively.
fn build_project_page(project: &ProjectRow, options: &RenderOptions) -> NormalizedChat {
    let project_uuid = project.project_uuid.clone();
    let page_uuid = ids::project(&project_uuid).uuid;
    let name = project
        .name
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "(untitled project)".to_string());

    // Anchor every synthesized section to the project's own timestamp so
    // the page is stable across runs; docs use their own `created_at`
    // where they have one. `+1` / `+2` keeps the description and the
    // instructions in that order under chat-common's sort.
    let base_ms = project
        .created_at
        .as_deref()
        .and_then(iso_to_ms)
        .or_else(|| project.updated_at.as_deref().and_then(iso_to_ms))
        .unwrap_or(0);

    let mut items: Vec<NormalizedChatItem> = Vec::new();
    if let Some(text) = project.description.clone().and_then(filter_nonempty) {
        items.push(project_item(
            ids::project_description(&project_uuid),
            "Description",
            "Project Description",
            base_ms + 1,
            text,
        ));
    }
    if let Some(text) = project.prompt_template.clone().and_then(filter_nonempty) {
        items.push(project_item(
            ids::project_instructions(&project_uuid),
            "Custom instructions",
            "Project Instructions",
            base_ms + 2,
            text,
        ));
    }
    for (i, doc) in project.docs.iter().enumerate() {
        let label = doc
            .file_name
            .clone()
            .and_then(filter_nonempty)
            .unwrap_or_else(|| "(unnamed document)".to_string());
        let ms = doc
            .created_at
            .as_deref()
            .and_then(iso_to_ms)
            .unwrap_or(base_ms + 3 + i as i64);
        // Knowledge docs are arbitrary user text — often markdown, and
        // fencing them would break that. Emitted verbatim, the same way
        // a chat message body is; the section header carries the file
        // name. Bounded, though: see `max_project_doc_bytes`.
        let body = doc
            .content
            .as_deref()
            .map(|c| clamp_doc_text(c, options.max_project_doc_bytes))
            .and_then(filter_nonempty);
        let doc_id = ids::project_document(&doc.doc_uuid);
        items.push(NormalizedChatItem {
            message_uuid: doc_id.uuid.clone(),
            author_id: "project_doc".into(),
            author_display: label,
            date_ms: ms,
            text: body,
            kind: ItemKind::Text,
            attachments: Vec::new(),
            reactions: Vec::new(),
            system_note: None,
            source_url: None,
            kind_label: Some("Project Knowledge".to_string()),
            source_ref: Some(UpstreamRef::new(
                doc_id.entity_kind,
                doc_id.natural_key.clone(),
            )),
        });
    }
    items.sort_by_key(|i| i.date_ms);

    NormalizedChat {
        id: page_uuid.clone(),
        chat_uuid: page_uuid.clone(),
        display: name.clone(),
        // Distinguishes a project page from a chat page at a glance;
        // without it chat-common derives the same "Claude · {name}"
        // heading it gives conversations.
        title: Some(format!("Claude Project · {name}")),
        account: filter_nonempty(project.account_uuid.clone()),
        // A project's own `project` column is itself, so the grid groups
        // the project page together with its conversations.
        project: Some(name),
        // The project's own UUID — same round-trip role as a
        // conversation's, see `build_chat`.
        external_id: Some(project_uuid.clone()),
        source_url: Some(format!("https://claude.ai/project/{project_uuid}")),
        upstream_scope: None,
        org_uuid: project.org_uuid.clone(),
        org_name: project.org_name.clone(),
        buckets: vec![NormalizedDoc {
            period_key: "all".to_string(),
            markdown_uuid: page_uuid,
            items,
        }],
    }
}

/// Bound one knowledge document's inline text, appending a visible
/// marker when it is cut. Truncates on a char boundary so the result is
/// still valid UTF-8, and says how much was dropped so a reader knows
/// to raise the ceiling (or open the source) rather than assuming the
/// document ends there.
fn clamp_doc_text(content: &str, max_bytes: Option<usize>) -> String {
    let Some(max) = max_bytes else {
        return content.to_string();
    };
    if content.len() <= max {
        return content.to_string();
    }
    let mut cut = max;
    while cut > 0 && !content.is_char_boundary(cut) {
        cut -= 1;
    }
    format!(
        "{}\n\n*[truncated: showing {} of {} bytes — raise \
         `max_project_doc_bytes` to see more; the raw store has all of it]*",
        &content[..cut],
        cut,
        content.len()
    )
}

/// One synthesized project section (description / custom instructions).
fn project_item(
    id: ids::Identity,
    author_display: &str,
    kind_label: &str,
    date_ms: i64,
    text: String,
) -> NormalizedChatItem {
    NormalizedChatItem {
        message_uuid: id.uuid,
        author_id: kind_label.to_string(),
        author_display: author_display.to_string(),
        date_ms,
        text: Some(text),
        kind: ItemKind::Text,
        attachments: Vec::new(),
        reactions: Vec::new(),
        system_note: None,
        source_url: None,
        kind_label: Some(kind_label.to_string()),
        source_ref: Some(UpstreamRef::new(id.entity_kind, id.natural_key)),
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

/// Identity for one structural block: its grid-row `uuid` (also the
/// item's `message_uuid` and its `data-section-uuid` anchor), plus the
/// upstream id and entity kind that produced it.
///
/// A `tool_use` is keyed on its own `id`, a `tool_result` on the
/// `tool_use_id` it answers — that is how Anthropic links the pair —
/// and both are scoped to the containing message. A `thinking` block
/// has no upstream id, so its position within the message is the key.
/// When a block is missing the id its type calls for, position is the
/// fallback.
///
/// Replaces the old `tu-`/`tr-`/`th-`/`blk-` string prefixes; see
/// [`super::ids`] for what those got wrong.
pub(crate) fn block_identity(
    msg_uuid: &str,
    block_index: usize,
    btype: &str,
    raw_obj: &serde_json::Map<String, Value>,
) -> ids::Identity {
    let field = match btype {
        "tool_use" => "id",
        "tool_result" => "tool_use_id",
        _ => "",
    };
    let upstream = (!field.is_empty())
        .then(|| raw_obj.get(field).and_then(Value::as_str))
        .flatten();
    match (btype, upstream) {
        ("tool_use", Some(id)) => ids::tool_use(msg_uuid, id),
        ("tool_result", Some(id)) => ids::tool_result(msg_uuid, id),
        ("thinking", _) => ids::thinking_block(msg_uuid, block_index),
        // A tool block whose id field is absent. Position is all that
        // is left, and it is still stable for a given message.
        _ => ids::block_fallback(msg_uuid, block_index),
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

#[cfg(test)]
mod project_doc_tests {
    use super::*;

    #[test]
    fn short_docs_are_untouched() {
        assert_eq!(clamp_doc_text("hello", Some(128)), "hello");
        assert_eq!(clamp_doc_text("hello", None), "hello");
    }

    /// The whole point of the ceiling: a book-sized knowledge doc must
    /// not reach the page (or the grid row) at full length.
    #[test]
    fn long_docs_are_cut_and_say_so() {
        let big = "x".repeat(10_000);
        let out = clamp_doc_text(&big, Some(100));
        assert!(
            out.len() < 400,
            "expected a bounded result, got {}",
            out.len()
        );
        assert!(out.starts_with(&"x".repeat(100)));
        assert!(
            out.contains("truncated: showing 100 of 10000 bytes"),
            "a reader has to be able to tell the doc was cut: {out}"
        );
    }

    /// Cutting mid-codepoint would produce invalid UTF-8; we back up to
    /// the previous boundary instead. 'é' is two bytes, so a limit of 5
    /// lands inside the third one.
    #[test]
    fn cuts_on_a_char_boundary() {
        let s = "ééé";
        assert_eq!(s.len(), 6);
        let out = clamp_doc_text(s, Some(5));
        assert!(out.starts_with("éé"), "got {out:?}");
        assert!(out.contains("showing 4 of 6 bytes"), "got {out:?}");
    }

    /// A zero ceiling keeps the marker rather than emitting an empty
    /// section, so the doc still shows up as existing.
    #[test]
    fn zero_ceiling_still_names_the_document() {
        let out = clamp_doc_text("anything", Some(0));
        assert!(out.contains("showing 0 of 8 bytes"), "got {out:?}");
    }
}
