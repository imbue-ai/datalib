//! Claude render: convert parsed conversations into the
//! shared `chat-common` normalized model and delegate markdown /
//! grid-row plumbing to
//! [`datalib_etl_chat_common::render::render_all`].

use std::collections::HashMap;

use anyhow::{Context as _, Result};
use serde_json::Value;

use datalib_etl::blob_cas::BlobBundle;
use datalib_etl::progress::Progress;
use datalib_etl_chat_common::branches::{reading_order, TreeNode};
use datalib_etl_chat_common::normalize::{capitalize, iso_to_ms, json_pretty_sorted};
use datalib_etl_chat_common::render::{
    render_all as cc_render_all, Buckets, RenderProfile, ENTITY_KIND_CONVERSATION,
};
use datalib_etl_chat_common::types::{
    own_stamp_ms, ItemKind, NormalizedAttachment, NormalizedChat, NormalizedChatItem,
    NormalizedDoc, UpstreamRef,
};
use datalib_etl_chat_common::TextFormat;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::html::{
    escape_md_block, escape_md_inline, escape_text, md_code_block, md_code_span, md_link_dest,
};
use datalib_etl_render::inputs::Inputs;
use datalib_etl_render::sources::Source;

use super::ids;
use super::parse::{
    shred, AttachmentRow, ContentBlockRow, MessageRow, ParsedExport, ProjectRow,
    ShreddedConversation,
};
use datalib_id::Identity;
use datalib_schema::providers::Provider;

/// Bump when the item-shape / column mapping changes meaningfully.
/// v3: render via chat-common (block-explosion).
/// v4: projects render as their own pages, and a conversation's
///     `project` grid column carries the project name, not its UUID.
/// v5: ids are minted through `datalib_id` instead of passing
///     Anthropic's through (#216). Every uuid this renderer emits
///     changed, `chat_uuid` among them — and `chat_uuid` names the
///     output directory, so a tree written by v4 cannot be updated in
///     place. The render step discards it wholesale; see
///     `DataProcessor::render_version`.
/// v6: an item with no timestamp — and no parent stamp to inherit from
///     — gets a null `created_at` instead of a real-looking
///     `1970-01-01T00:00:00`. See
///     `docs/dev/data_architecture_parse_and_render.md` §6.
/// v7: `account` is the account's email rather than Anthropic's user
///     UUID, and a project page carries the account that downloaded it
///     rather than its creator — who moves to `author`.
/// v8: every id carries its row's `created_at` in its leading bits
///     (`datalib_id`'s v8 layout), so a sync's rows land in adjacent
///     leaves of the render store and the index.
/// v10: the branch the user last saw rather than every edit; cited
///     sources listed; pasted text named; `artifacts`, `create_file`,
///     `bash_tool` and search results readable rather than JSON.
pub const RENDER_VERSION: u32 = 10;

fn profile() -> RenderProfile {
    RenderProfile {
        stamp_precision: ids::STAMP_PRECISION,
        provider: Provider::Claude,
        source_label: "Claude".to_string(),
        chat_kind: "Chat".to_string(),
        // Per-item kind is always set via `kind_label`; nominal fallback.
        message_kind: "LLM Response".to_string(),
        reaction_kind: "Claude Reaction".to_string(),
        chat_entity_kind: ENTITY_KIND_CONVERSATION,
        render_version: RENDER_VERSION,
        text_format: TextFormat::Markdown,
    }
}

/// Projects are not chats, but they are *page-shaped* in exactly the
/// way chat-common already handles: a titled page whose body is a list
/// of anchored sections, each with its own grid row. Reusing the same
/// renderer gets the `id="m-{uuid}"` / `data-section-uuid` anchors and
/// the rows for free — see docs/dev/cards.md
/// for why those anchors are load-bearing.
fn project_profile() -> RenderProfile {
    RenderProfile {
        stamp_precision: ids::STAMP_PRECISION,
        provider: Provider::Claude,
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
        text_format: TextFormat::Markdown,
    }
}

/// Render-time knobs. Separate from the config struct so the render
/// layer doesn't depend on the config crate.
#[derive(Debug, Clone, Copy)]
pub struct RenderOptions {
    /// See [`datalib_etl_claude_config::ClaudeRenderConfig::max_project_doc_bytes`].
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
    source_id: &str,
    options: RenderOptions,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<Buckets> {
    let elapsed_ms = parsed.scan.scan_elapsed.map(|d| d.as_millis() as u64);
    tracing::info!(
        source = source_id,
        scan_elapsed_ms = elapsed_ms,
        buckets_to_render = parsed
            .scan
            .render
            .as_ref()
            .map(|s| s.len() as i64)
            .unwrap_or(-1),
        conversations = parsed.conversations.len(),
        projects = parsed.projects.len(),
        cold_start = parsed.scan.render.is_none(),
        "[render] claude dolt_diff scan"
    );

    let mut chats: Vec<NormalizedChat> = Vec::with_capacity(parsed.conversations.len());
    let mut blobs_by_chat: HashMap<String, BlobBundle> = HashMap::new();
    for c in &parsed.conversations {
        let shredded = shred(c);
        let chat = build_chat(
            source_id,
            &shredded,
            &c.inputs,
            &parsed.project_name_by_uuid,
            parsed,
        );
        blobs_by_chat.insert(chat.id.clone(), c.blobs.clone());
        chats.push(chat);
    }
    let mut buckets = cc_render_all(
        &profile(),
        &chats,
        root,
        source_id,
        &blobs_by_chat,
        progress,
        on_doc_complete,
    )
    .context("claude chat-common render")?
    .buckets;

    // Projects are a second pass with their own profile. They share the
    // page-path namespace with conversations (`render_markdown/<source>/
    // <uuid>/all.md`) and can't collide: a project UUID is never a
    // conversation UUID. No blobs — knowledge docs carry their text
    // inline.
    if !parsed.projects.is_empty() {
        let project_chats: Vec<NormalizedChat> = parsed
            .projects
            .iter()
            .map(|p| build_project_page(source_id, p, &options, parsed))
            .collect();
        let no_blobs: HashMap<String, BlobBundle> = HashMap::new();
        let projects = cc_render_all(
            &project_profile(),
            &project_chats,
            root,
            source_id,
            &no_blobs,
            progress,
            on_doc_complete,
        )
        .context("claude project render")?;
        buckets.extend(projects.buckets);
    }

    Ok(buckets)
}

fn build_chat(
    source_id: &str,
    shredded: &ShreddedConversation,
    inputs: &Inputs,
    project_names: &HashMap<String, String>,
    parsed: &ParsedExport,
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
    let leaf = conv
        .raw_json
        .get("current_leaf_message_uuid")
        .and_then(Value::as_str);
    let msgs = in_reading_order(&shredded.messages, leaf);

    let mut items: Vec<NormalizedChatItem> = Vec::new();
    let mut last_ms = conv.created_at.as_deref().and_then(iso_to_ms);
    for (m, branch) in &msgs {
        // Own stamp, else the previous item's + 1ms (§6's sanctioned
        // inheritance, which keeps ordering stable across re-runs),
        // else nothing — a conversation whose own `created_at` is
        // missing and whose first message has none either genuinely has
        // no time to report, and `None` says so.
        let mut msg_problems = Vec::new();
        let msg_ms = own_stamp_ms(
            m.created_at.as_deref(),
            "created_at",
            iso_to_ms,
            &mut msg_problems,
        )
        .or_else(|| last_ms.map(|p| p + 1));
        last_ms = msg_ms.or(last_ms);

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
        let text_blocks: Vec<&ContentBlockRow> = blocks
            .iter()
            .copied()
            .filter(|b| b.r#type.as_deref() == Some("text"))
            .collect();
        let mut body_parts: Vec<String> = text_blocks
            .iter()
            .filter_map(|b| b.text.as_deref())
            .filter(|s| !s.is_empty())
            .map(|s| s.trim_end().to_string())
            .collect();
        body_parts.extend(sources_list(&text_blocks));

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
                body_parts.push(render_extracted_attachment(name.unwrap_or(""), extracted));
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
            // A block inherits its message's stamp, offset by its
            // index so blocks keep their order. `None` only when the
            // message had none either.
            let mut block_problems = Vec::new();
            let block_ms = own_stamp_ms(
                b.start_timestamp.as_deref(),
                "start_timestamp",
                iso_to_ms,
                &mut block_problems,
            )
            .or_else(|| msg_ms.map(|ms| ms + (b.block_index as i64) + 1));
            let block_id = block_identity(
                source_id,
                &m.message_uuid,
                b.block_index,
                btype,
                &raw_obj,
                block_ms,
            );
            let block_author = filter_nonempty(model.clone()).unwrap_or_else(|| btype.to_string());
            let body = block_body_md(btype, b.text.as_deref(), &raw_obj);
            items.push(NormalizedChatItem {
                message_uuid: block_id.uuid.clone(),
                author_handle: None,
                author_display: block_author,
                date_ms: block_ms,
                text: filter_nonempty(body),
                kind: ItemKind::Text,
                attachments: Vec::new(),
                reactions: Vec::new(),
                labels: Vec::new(),
                system_note: None,
                source_url: None,
                kind_label: Some(kind_for_block(btype).to_string()),
                source_ref: Some(UpstreamRef::new(
                    block_id.entity_kind,
                    block_id.natural_key.clone(),
                )),
                is_aside: matches!(btype, "tool_use" | "tool_result"),
                branch: branch.clone(),
                unread: false,
                recipients: Vec::new(),
                problems: block_problems,
            });
        }

        // The message's own item: its text blocks + extracted-text
        // attachments + downloadable files. Always emitted (even empty)
        // so the per-message grid row survives.
        let msg_id = ids::message(source_id, &m.message_uuid, msg_ms);
        let body = body_parts.join("\n\n");
        let kind = if norm_atts.is_empty() {
            ItemKind::Text
        } else {
            ItemKind::Attachment
        };
        items.push(NormalizedChatItem {
            message_uuid: msg_id.uuid.clone(),
            author_handle: None,
            author_display: author_display.clone(),
            date_ms: msg_ms,
            text: filter_nonempty(body),
            kind,
            attachments: norm_atts,
            reactions: Vec::new(),
            labels: Vec::new(),
            system_note: None,
            source_url: None,
            kind_label: Some(kind_label.to_string()),
            source_ref: Some(UpstreamRef::new(
                msg_id.entity_kind,
                msg_id.natural_key.clone(),
            )),
            is_aside: false,
            branch: branch.clone(),
            unread: false,
            recipients: Vec::new(),
            problems: msg_problems,
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
    let chat_uuid = ids::conversation(source_id, &conv_uuid).uuid;
    // The account row and the project row are looked up per chat, and
    // declared as they are.
    inputs.read("users", &conv.account_uuid);
    let project_names = inputs.lookup("projects", project_names);
    NormalizedChat {
        path_prefix: None,
        id: chat_uuid.clone(),
        chat_uuid: chat_uuid.clone(),
        display: title.clone(),
        title: Some(title),
        author: None,
        account: parsed.account_label(&conv.account_uuid),
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
        upstream_account: None,
        org_uuid: conv.org_uuid.clone(),
        org_name: conv.org_name.clone(),
        buckets: vec![NormalizedDoc {
            orphan_reactions: Vec::new(),
            period_key: "all".to_string(),
            markdown_uuid: chat_uuid,
            source_ref: None,
            items,
        }],
        contacts: Vec::new(),
        inputs: inputs.declared(),
    }
}

fn build_project_page(
    source_id: &str,
    project: &ProjectRow,
    options: &RenderOptions,
    parsed: &ParsedExport,
) -> NormalizedChat {
    let project_uuid = project.project_uuid.clone();
    let page_uuid = ids::project(source_id, &project_uuid).uuid;
    let name = project
        .name
        .clone()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "(untitled project)".to_string());

    // Anchor every synthesized section to the project's own timestamp so
    // the page is stable across runs; docs use their own `created_at`
    // where they have one. `+1` / `+2` keeps the description and the
    // instructions in that order under chat-common's sort.
    // `None` when the project carries neither stamp: the synthesized
    // sections below then have no time to report, and say so.
    let base_ms = project
        .created_at
        .as_deref()
        .and_then(iso_to_ms)
        .or_else(|| project.updated_at.as_deref().and_then(iso_to_ms));

    let mut items: Vec<NormalizedChatItem> = Vec::new();
    if let Some(text) = project.description.clone().and_then(filter_nonempty) {
        items.push(project_item(
            ids::project_description(source_id, &project_uuid, base_ms.map(|b| b + 1)),
            "Description",
            "Project Description",
            text,
        ));
    }
    if let Some(text) = project.prompt_template.clone().and_then(filter_nonempty) {
        items.push(project_item(
            ids::project_instructions(source_id, &project_uuid, base_ms.map(|b| b + 2)),
            "Custom instructions",
            "Project Instructions",
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
            .or_else(|| base_ms.map(|b| b + 3 + i as i64));
        // Knowledge docs are arbitrary user text — often markdown, and
        // fencing them would break that. Emitted verbatim, the same way
        // a chat message body is; the section header carries the file
        // name. Bounded, though: see `max_project_doc_bytes`.
        let body = doc
            .content
            .as_deref()
            .map(|c| clamp_doc_text(c, options.max_project_doc_bytes))
            .and_then(filter_nonempty);
        let doc_id = ids::project_document(source_id, &doc.doc_uuid, ms);
        items.push(NormalizedChatItem {
            message_uuid: doc_id.uuid.clone(),
            author_handle: None,
            author_display: label,
            date_ms: ms,
            text: body,
            kind: ItemKind::Text,
            attachments: Vec::new(),
            reactions: Vec::new(),
            labels: Vec::new(),
            system_note: None,
            source_url: None,
            kind_label: Some("Project Knowledge".to_string()),
            source_ref: Some(UpstreamRef::new(
                doc_id.entity_kind,
                doc_id.natural_key.clone(),
            )),
            is_aside: false,
            branch: Vec::new(),
            unread: false,
            recipients: Vec::new(),
            problems: Vec::new(),
        });
    }
    items.sort_by_key(|i| i.date_ms);

    if let Some(viewer) = &parsed.viewer_account_uuid {
        project.inputs.read("users", viewer);
    }
    NormalizedChat {
        path_prefix: None,
        id: page_uuid.clone(),
        chat_uuid: page_uuid.clone(),
        display: name.clone(),
        // Distinguishes a project page from a chat page at a glance;
        // without it chat-common derives the same "Claude · {name}"
        // heading it gives conversations.
        title: Some(format!("Claude Project · {name}")),
        author: project.creator_name.clone(),
        // The account that downloaded the project, not the colleague who
        // created it: a shared Team project is here because of the same
        // login every conversation is.
        account: parsed.viewer_account_label(),
        // A project's own `project` column is itself, so the grid groups
        // the project page together with its conversations.
        project: Some(name),
        // The project's own UUID — same round-trip role as a
        // conversation's, see `build_chat`.
        external_id: Some(project_uuid.clone()),
        source_url: Some(format!("https://claude.ai/project/{project_uuid}")),
        upstream_account: None,
        org_uuid: project.org_uuid.clone(),
        org_name: project.org_name.clone(),
        buckets: vec![NormalizedDoc {
            orphan_reactions: Vec::new(),
            period_key: "all".to_string(),
            markdown_uuid: page_uuid,
            source_ref: None,
            items,
        }],
        contacts: Vec::new(),
        inputs: project.inputs.declared(),
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

fn project_item(
    id: Identity,
    author_display: &str,
    kind_label: &str,
    text: String,
) -> NormalizedChatItem {
    NormalizedChatItem {
        message_uuid: id.uuid,
        author_handle: None,
        author_display: author_display.to_string(),
        date_ms: id.at,
        text: Some(text),
        kind: ItemKind::Text,
        attachments: Vec::new(),
        reactions: Vec::new(),
        labels: Vec::new(),
        system_note: None,
        source_url: None,
        kind_label: Some(kind_label.to_string()),
        source_ref: Some(UpstreamRef::new(id.entity_kind, id.natural_key)),
        is_aside: false,
        branch: Vec::new(),
        unread: false,
        recipients: Vec::new(),
        problems: Vec::new(),
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

/// Every message in reading order with the branches it sits in: the
/// branch ending at the leaf the user last saw, every other version
/// folded in where it forked. With no usable leaf (an export-shaped
/// payload has none), every message by time on one branch.
fn in_reading_order<'a>(
    messages: &'a [MessageRow],
    leaf: Option<&str>,
) -> Vec<(&'a MessageRow, Vec<String>)> {
    let mut by_time: Vec<&MessageRow> = messages.iter().collect();
    by_time.sort_by(|a, b| {
        (
            a.created_at.as_deref().unwrap_or(""),
            a.message_uuid.as_str(),
        )
            .cmp(&(
                b.created_at.as_deref().unwrap_or(""),
                b.message_uuid.as_str(),
            ))
    });
    let nodes: Vec<TreeNode<'_>> = by_time
        .iter()
        .map(|m| TreeNode {
            id: &m.message_uuid,
            parent: m.parent_message_uuid.as_deref(),
        })
        .collect();
    let Some(order) = leaf.and_then(|leaf| reading_order(&nodes, leaf)) else {
        return by_time.into_iter().map(|m| (m, Vec::new())).collect();
    };
    let by_uuid: HashMap<&str, &MessageRow> = by_time
        .iter()
        .map(|m| (m.message_uuid.as_str(), *m))
        .collect();
    order
        .into_iter()
        .map(|p| {
            (
                by_uuid[p.id],
                p.branch.iter().map(|b| b.to_string()).collect(),
            )
        })
        .collect()
}

/// Every page a message's text cites, once each in order of first
/// citation, as a numbered list after the text. A citation names its
/// spans by character offset; splicing markers into the text there is
/// not attempted.
fn sources_list(text_blocks: &[&ContentBlockRow]) -> Option<String> {
    let citations = text_blocks
        .iter()
        .filter_map(|b| b.raw_json.get("citations").and_then(Value::as_array))
        .flatten()
        .filter_map(Value::as_object);
    let mut cited: Vec<Source<'_>> = Vec::new();
    for c in citations {
        let site_of_citation = c
            .get("metadata")
            .and_then(|m| m.get("site_name"))
            .and_then(Value::as_str);
        let sources: Vec<&serde_json::Map<String, Value>> = c
            .get("sources")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_object).collect())
            .unwrap_or_default();
        if sources.is_empty() {
            if let Some(url) = c.get("url").and_then(Value::as_str) {
                cited.push(Source {
                    url,
                    title: str_of(c, "title"),
                    site: site_of_citation,
                });
            }
            continue;
        }
        cited.extend(sources.iter().filter_map(|s| {
            let url = str_of(s, "url")?;
            let site = str_of(s, "source").or_else(|| {
                (c.get("url").and_then(Value::as_str) == Some(url))
                    .then_some(site_of_citation)
                    .flatten()
            });
            Some(Source {
                url,
                title: str_of(s, "title").or_else(|| str_of(c, "title")),
                site,
            })
        }));
    }
    datalib_etl_render::sources::sources_list(cited)
}

/// A string field, `None` when absent or empty.
fn str_of<'a>(o: &'a serde_json::Map<String, Value>, key: &str) -> Option<&'a str> {
    o.get(key).and_then(Value::as_str).filter(|s| !s.is_empty())
}

// Block / attachment rendering (the markdown that becomes item.text).

pub(crate) fn block_identity(
    source_id: &str,
    msg_uuid: &str,
    block_index: usize,
    btype: &str,
    raw_obj: &serde_json::Map<String, Value>,
    date_ms: Option<i64>,
) -> Identity {
    let field = match btype {
        "tool_use" => "id",
        "tool_result" => "tool_use_id",
        _ => "",
    };
    let upstream = (!field.is_empty())
        .then(|| raw_obj.get(field).and_then(Value::as_str))
        .flatten();
    match (btype, upstream) {
        ("tool_use", Some(id)) => ids::tool_use(source_id, msg_uuid, id, date_ms),
        ("tool_result", Some(id)) => ids::tool_result(source_id, msg_uuid, id, date_ms),
        ("thinking", _) => ids::thinking_block(source_id, msg_uuid, block_index, date_ms),
        // A tool block whose id field is absent. Position is all that
        // is left, and it is still stable for a given message.
        _ => ids::block_fallback(source_id, msg_uuid, block_index, date_ms),
    }
}

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
                Some(m) => format!("Tool use: {} — {}", escape_text(name), escape_text(m)),
                None => format!("Tool use: {}", escape_text(name)),
            };
            let mut out = vec![
                format!("<details><summary>{summary}</summary>"),
                String::new(),
            ];
            if let Some(tool_input) = raw_obj.get("input") {
                out.extend(tool_input_md(name, tool_input));
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
            let name = escape_text(name);
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

/// A tool's output is shown as it came back, in a code block, whether it
/// is one string or a list of text parts: it is the tool's, and nothing
/// in it is markup for this page.
fn render_tool_result_content(content: Option<&Value>, out: &mut Vec<String>) {
    match content {
        Some(Value::String(s)) => {
            out.push(md_code_block("", s.trim_end()));
        }
        Some(Value::Array(items)) => {
            let mut in_list = false;
            for item in items {
                if let Some(link) = item.as_object().and_then(knowledge_link) {
                    out.push(link);
                    in_list = true;
                    continue;
                }
                if std::mem::take(&mut in_list) {
                    out.push(String::new());
                }
                match item {
                    Value::Object(m)
                        if m.get("type").and_then(Value::as_str) == Some("text")
                            && m.get("text")
                                .and_then(Value::as_str)
                                .is_some_and(|t| !t.is_empty()) =>
                    {
                        let text = m.get("text").and_then(Value::as_str).unwrap();
                        out.push(md_code_block("", text.trim_end()));
                        out.push(String::new());
                    }
                    Value::Object(_) => {
                        out.push("```json".into());
                        out.push(json_pretty_sorted(item));
                        out.push("```".into());
                        out.push(String::new());
                    }
                    other => {
                        let text = match other {
                            Value::String(s) => s.clone(),
                            v => v.to_string(),
                        };
                        out.push(md_code_block("", text.trim_end()));
                        out.push(String::new());
                    }
                }
            }
            if in_list {
                out.push(String::new());
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

/// A page a search or fetch tool came back with, as a link bullet. Only
/// a `knowledge` item with a url and no `text` is one; one that carries
/// text keeps the JSON fallback, which shows it.
fn knowledge_link(item: &serde_json::Map<String, Value>) -> Option<String> {
    if item.get("type").and_then(Value::as_str) != Some("knowledge")
        || str_of(item, "text").is_some()
    {
        return None;
    }
    let url = str_of(item, "url")?;
    let mut line = format!(
        "- [{}]({})",
        escape_md_inline(str_of(item, "title").unwrap_or(url)),
        md_link_dest(url)
    );
    let site = item
        .get("metadata")
        .and_then(Value::as_object)
        .and_then(|m| str_of(m, "site_name").or_else(|| str_of(m, "site_domain")));
    if let Some(site) = site {
        line.push_str(&format!(" — {}", escape_md_inline(site)));
    }
    Some(line)
}

/// A well-known tool's input, readable; any other as JSON.
fn tool_input_md(name: &str, input: &Value) -> Vec<String> {
    let readable = input.as_object().and_then(|i| match name {
        "artifacts" => artifact_md(i),
        "create_file" => file_md(i, "path", "file_text"),
        "Write" => file_md(i, "file_path", "content"),
        "Edit" => edit_md(i),
        "Read" => str_of(i, "file_path").map(|p| format!("Read {}", md_code_span(p))),
        "bash_tool" | "Bash" => bash_md(i),
        _ => None,
    });
    match readable {
        Some(md) => vec![md],
        None if json_is_empty(input) => Vec::new(),
        None => vec!["```json".into(), json_pretty_sorted(input), "```".into()],
    }
}

fn artifact_md(i: &serde_json::Map<String, Value>) -> Option<String> {
    let kind = str_of(i, "type");
    let lang = str_of(i, "language")
        .or_else(|| kind.and_then(artifact_language))
        .unwrap_or("");
    let body = match (
        str_of(i, "content"),
        i.get("old_str").and_then(Value::as_str),
        i.get("new_str").and_then(Value::as_str),
    ) {
        (Some(content), _, _) => md_code_block(lang, content.trim_end_matches('\n')),
        (None, Some(old), Some(new)) => format!(
            "Replace:\n\n{}\n\nwith:\n\n{}",
            md_code_block(lang, old),
            md_code_block(lang, new)
        ),
        _ => return None,
    };
    let mut head = format!(
        "{} artifact",
        capitalize(str_of(i, "command").unwrap_or("create"))
    );
    if let Some(title) = str_of(i, "title").or_else(|| str_of(i, "id")) {
        head.push_str(&format!(": **{}**", escape_md_inline(title)));
    }
    if let Some(kind) = kind {
        head.push_str(&format!(" ({})", md_code_span(kind)));
    }
    Some(format!("{head}\n\n{body}"))
}

/// The fence language for an artifact type that names no `language`.
fn artifact_language(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "text/markdown" => "markdown",
        "text/html" => "html",
        "image/svg+xml" => "svg",
        "application/vnd.ant.mermaid" => "mermaid",
        "application/vnd.ant.react" => "jsx",
        _ => return None,
    })
}

/// A file written whole: `create_file` names its fields `path` and
/// `file_text`, the sandbox's `Write` `file_path` and `content`.
fn file_md(i: &serde_json::Map<String, Value>, path_key: &str, text_key: &str) -> Option<String> {
    let path = str_of(i, path_key)?;
    let text = i.get(text_key).and_then(Value::as_str)?;
    let lang = language_of(path);
    let mut head = format!("Create {}", md_code_span(path));
    if let Some(d) = str_of(i, "description") {
        head.push_str(&format!(" — {}", escape_md_inline(d)));
    }
    Some(format!(
        "{head}\n\n{}",
        md_code_block(lang, text.trim_end_matches('\n'))
    ))
}

fn edit_md(i: &serde_json::Map<String, Value>) -> Option<String> {
    let path = str_of(i, "file_path")?;
    let old = i.get("old_string").and_then(Value::as_str)?;
    let new = i.get("new_string").and_then(Value::as_str)?;
    let lang = language_of(path);
    Some(format!(
        "Edit {}\n\nReplace:\n\n{}\n\nwith:\n\n{}",
        md_code_span(path),
        md_code_block(lang, old),
        md_code_block(lang, new)
    ))
}

/// A fence language from a path's extension, the extension itself when
/// it names none of these.
fn language_of(path: &str) -> &str {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext.to_ascii_lowercase().as_str() {
        "py" => "python",
        "md" => "markdown",
        "rs" => "rust",
        "js" => "javascript",
        "ts" => "typescript",
        "sh" => "bash",
        _ => ext,
    }
}

fn bash_md(i: &serde_json::Map<String, Value>) -> Option<String> {
    let block = md_code_block("bash", str_of(i, "command")?);
    Some(match str_of(i, "description") {
        Some(d) => format!("{}\n\n{block}", escape_md_inline(d)),
        None => block,
    })
}

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
    // Pasted text arrives as an attachment with an empty `file_name`.
    let header_label = if label.is_empty() {
        "Pasted text".to_string()
    } else {
        escape_md_inline(label)
    };
    let body = extracted.unwrap_or("").trim();
    if body.is_empty() {
        return format!("**[attachment: {header_label}]** *(no extracted content)*");
    }
    let quoted: String = escape_md_block(body)
        .lines()
        .map(|l| format!("> {l}\n"))
        .collect();
    format!("**[attachment: {header_label}]**\n{quoted}")
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

#[cfg(test)]
mod escaping_tests {
    use super::*;
    use serde_json::json;

    const MARKUP: &str = "<script>x</script> & co";

    /// A tool's name, its output and an upload's text came from the tool
    /// or the file, not from the page: none of it may open a tag.
    #[test]
    fn tool_traffic_in_markup_renders_escaped() {
        let used = json!({"name": MARKUP, "message": MARKUP, "input": {}});
        let md = block_body_md("tool_use", None, used.as_object().unwrap());
        assert!(
            md.starts_with(
                "<details><summary>Tool use: &lt;script&gt;x&lt;/script&gt; &amp; co — \
                 &lt;script&gt;x&lt;/script&gt; &amp; co</summary>"
            ),
            "{md}"
        );

        let result = json!({
            "name": MARKUP,
            "content": [{"type": "text", "text": "```\n</details>\n```"}],
        });
        let md = block_body_md("tool_result", None, result.as_object().unwrap());
        assert!(md.contains("Tool result: &lt;script&gt;"), "{md}");
        assert!(
            md.contains("````\n```\n</details>\n```\n````"),
            "the output stays inside its own fence: {md}"
        );

        let md = render_extracted_attachment(MARKUP, Some(MARKUP));
        assert_eq!(
            md,
            "**[attachment: &lt;script&gt;x&lt;/script&gt; &amp; co]**\n\
             > &lt;script&gt;x&lt;/script&gt; &amp; co\n"
        );
    }
}

#[cfg(test)]
mod branch_tests {
    use super::*;

    fn msg(uuid: &str, parent: Option<&str>, at: &str) -> MessageRow {
        MessageRow {
            conversation_uuid: "c".into(),
            message_uuid: uuid.into(),
            parent_message_uuid: parent.map(str::to_string),
            sender: Some("human".into()),
            text: None,
            created_at: Some(at.into()),
            updated_at: None,
            raw_json: Value::Null,
        }
    }

    fn placed(msgs: &[MessageRow], leaf: Option<&str>) -> Vec<(String, String)> {
        in_reading_order(msgs, leaf)
            .into_iter()
            .map(|(m, branch)| (m.message_uuid.clone(), branch.join("/")))
            .collect()
    }

    /// An edited prompt starts a branch. The edit left behind, and its
    /// answer, are kept on their own branch just before the edit that
    /// replaced them, where they used to read as if sent in turn.
    #[test]
    fn the_edit_left_behind_is_kept_on_its_own_branch() {
        let msgs = [
            msg("q1", Some("00000000-0000-4000-8000-000000000000"), "1"),
            msg("a1", Some("q1"), "2"),
            msg("q2-old", Some("a1"), "3"),
            msg("a2-old", Some("q2-old"), "4"),
            msg("q2", Some("a1"), "5"),
            msg("a2", Some("q2"), "6"),
        ];
        let on = |id: &str, b: &str| (id.to_string(), b.to_string());
        assert_eq!(
            placed(&msgs, Some("a2")),
            [
                on("q1", ""),
                on("a1", ""),
                on("q2-old", "q2-old"),
                on("a2-old", "q2-old"),
                on("q2", ""),
                on("a2", ""),
            ]
        );
    }

    /// An export-shaped payload has no leaf, and a chain can loop: every
    /// message, by time, on one branch.
    #[test]
    fn no_leaf_or_a_loop_reads_every_message_by_time() {
        let msgs = [msg("q1", None, "1"), msg("a1", Some("q1"), "2")];
        let flat = [
            ("q1".to_string(), String::new()),
            ("a1".to_string(), String::new()),
        ];
        assert_eq!(placed(&msgs, None), flat);
        assert_eq!(placed(&msgs, Some("nope")), flat);
        let looped = [msg("q1", Some("a1"), "1"), msg("a1", Some("q1"), "2")];
        assert_eq!(placed(&looped, Some("a1")).len(), 2);
    }
}

#[cfg(test)]
mod readable_tool_tests {
    use super::*;
    use serde_json::json;

    fn text_block(citations: Value) -> ContentBlockRow {
        ContentBlockRow {
            message_uuid: "m".into(),
            block_index: 0,
            r#type: Some("text".into()),
            text: Some("Shields at 40%.".into()),
            start_timestamp: None,
            stop_timestamp: None,
            raw_json: json!({"type": "text", "citations": citations}),
        }
    }

    /// Citations were dropped. Every source is listed once, by url, in
    /// order of first citation, its title escaped as link text.
    #[test]
    fn cited_sources_are_listed_once_each() {
        let one = text_block(json!([
            {"url": "https://ma.test/a", "title": "A", "metadata": {"site_name": "MA"},
             "sources": [{"url": "https://ma.test/a", "title": "Shields [ref]", "source": "Memory Alpha"}]},
            {"url": "https://ma.test/b", "title": "B", "metadata": {"site_name": "MB"},
             "sources": [
                {"url": "https://ma.test/b", "title": "B page"},
                {"url": "https://ma.test/a", "title": "again"}]},
            {"url": "https://ma.test/c (x)", "title": "C"},
            null,
        ]));
        let md = sources_list(&[&one]).unwrap();
        assert_eq!(
            md,
            "**Sources**\n\n\
             1. [Shields \\[ref\\]](https://ma.test/a) — Memory Alpha\n\
             2. [B page](https://ma.test/b) — MB\n\
             3. [C](<https://ma.test/c (x)>)"
        );
        assert!(sources_list(&[&text_block(json!([]))]).is_none());
    }

    /// A file the tool wrote may hold backticks of its own; the fence has
    /// to outlast them.
    #[test]
    fn an_artifact_renders_as_its_code_in_a_fence_it_cannot_close() {
        let input = json!({
            "command": "create", "title": "Warp <calc>", "type": "application/vnd.ant.code",
            "language": "python", "content": "doc = '''\n```\n'''",
        });
        let md = tool_input_md("artifacts", &input).join("\n");
        assert_eq!(
            md,
            "Create artifact: **Warp &lt;calc&gt;** (`application/vnd.ant.code`)\n\n\
             ````python\ndoc = '''\n```\n'''\n````"
        );
        let update = json!({"command": "update", "id": "warp", "old_str": "9.2", "new_str": "9.6",
                            "type": "text/markdown"});
        assert_eq!(
            tool_input_md("artifacts", &update).join("\n"),
            "Update artifact: **warp** (`text/markdown`)\n\n\
             Replace:\n\n```markdown\n9.2\n```\n\nwith:\n\n```markdown\n9.6\n```"
        );
    }

    /// The sandbox's file tools name their fields differently from the
    /// older `create_file` and `bash_tool`, and were left as JSON.
    #[test]
    fn the_sandbox_file_tools_read_as_code() {
        let write = json!({"file_path": "warp.py", "content": "print(9.6)\n"});
        assert_eq!(
            tool_input_md("Write", &write).join("\n"),
            "Create `warp.py`\n\n```python\nprint(9.6)\n```"
        );
        let edit = json!({"file_path": "/home/claude/coil.svg", "old_string": "red",
                          "new_string": "blue", "replace_all": false});
        assert_eq!(
            tool_input_md("Edit", &edit).join("\n"),
            "Edit `/home/claude/coil.svg`\n\nReplace:\n\n```svg\nred\n```\n\nwith:\n\n```svg\nblue\n```"
        );
        assert_eq!(
            tool_input_md("Read", &json!({"file_path": "/home/claude/log.txt"})).join("\n"),
            "Read `/home/claude/log.txt`"
        );
        assert_eq!(
            tool_input_md("Bash", &json!({"command": "ls"})).join("\n"),
            "```bash\nls\n```"
        );
    }

    /// A tool this render does not know, or a known one missing what it
    /// needs, keeps its JSON.
    #[test]
    fn anything_else_keeps_its_json() {
        let md = tool_input_md("bash_tool", &json!({"description": "no command"})).join("\n");
        assert!(md.starts_with("```json\n"), "{md}");
        let md = tool_input_md("replicator", &json!({"order": "tea"})).join("\n");
        assert!(md.starts_with("```json\n"), "{md}");
        assert!(tool_input_md("replicator", &json!({})).is_empty());
    }

    #[test]
    fn search_results_render_as_links() {
        let mut out = Vec::new();
        render_tool_result_content(
            Some(&json!([
                {"type": "knowledge", "title": "Warp <core>", "url": "https://ma.test/warp",
                 "metadata": {"site_name": "Memory Alpha"}},
                {"type": "knowledge", "title": "Dilithium", "url": "https://ma.test/d"},
                {"type": "text", "text": "done"},
            ])),
            &mut out,
        );
        assert_eq!(
            out.join("\n"),
            "- [Warp &lt;core&gt;](https://ma.test/warp) — Memory Alpha\n\
             - [Dilithium](https://ma.test/d)\n\n```\ndone\n```\n"
        );
    }
}
