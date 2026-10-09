//! ChatGPT render: convert parsed conversations into the shared
//! `chat-common` normalized model and delegate markdown / grid-row /
//! grid-row plumbing to [`datalib_etl_chat_common::render::render_all`].

use std::collections::HashMap;

use anyhow::{Context as _, Result};
use datalib_etl::blob_cas::BlobBundle;
use datalib_etl::progress::Progress;
use datalib_etl_chat_common::branches::{reading_order, TreeNode};
use datalib_etl_chat_common::normalize::{capitalize, iso_to_ms};
use datalib_etl_chat_common::render::{
    render_all as cc_render_all, Buckets, RenderProfile, ENTITY_KIND_CONVERSATION,
};
use datalib_etl_chat_common::types::{
    own_stamp_ms, ItemKind, NormalizedAttachment, NormalizedChat, NormalizedChatItem,
    NormalizedDoc, UpstreamRef,
};
use datalib_etl_chat_common::TextFormat;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::html::{escape_md_block, escape_md_inline, md_code_block};
use datalib_etl_render::sources::{sources_list, Source};

use super::ids;
use super::parse::{
    shred, OAAttachmentRef, OAContentPartRow, OAMessageRow, ParsedChatGPTApi, ShreddedConversation,
};
use datalib_schema::problems::{Problem, Reason};
use datalib_schema::providers::Provider;
use serde_json::Value;

/// Bump when the item-shape / column mapping changes meaningfully.
/// v4: render via chat-common.
/// v5: ids are minted through `datalib_id` instead of passing OpenAI's
///     through (#216). Every uuid this renderer emits changed,
///     `chat_uuid` among them — and `chat_uuid` names the output
///     directory, so a tree written by v4 cannot be updated in place.
///     The render step discards it wholesale; see
///     `DataProcessor::render_version`.
/// v6: a message with no `create_time` — and no previous item's stamp to
///     inherit from — gets a null `created_at` instead of a real-looking
///     `1970-01-01T00:00:00`. See
///     `docs/dev/data_architecture_parse_and_render.md` §6.
/// v8: `account` is the login's email rather than OpenAI's opaque
///     `user-…` id.
/// v9: every id carries its row's `created_at` in its leading bits
///     (`datalib_id`'s v8 layout).
/// v11: the private-use characters around a cited span are dropped
///     instead of showing as boxes.
/// v12: the words beside an image, quotes of uploaded files and named
///     entities are kept; empty steps are left out; cited pages are
///     listed after the text.
/// v13: a message is keyed within its conversation, since a branched
///     conversation repeats the original's message ids.
pub const RENDER_VERSION: u32 = 13;

fn profile() -> RenderProfile {
    RenderProfile {
        stamp_precision: ids::STAMP_PRECISION,
        provider: Provider::Chatgpt,
        source_label: "ChatGPT".to_string(),
        chat_kind: "Chat".to_string(),
        // Per-message kind is always set via `kind_label`; this is only a
        // nominal fallback.
        message_kind: "LLM Response".to_string(),
        reaction_kind: "ChatGPT Reaction".to_string(),
        chat_entity_kind: ENTITY_KIND_CONVERSATION,
        render_version: RENDER_VERSION,
        text_format: TextFormat::Markdown,
    }
}

pub fn render_all(
    parsed: &ParsedChatGPTApi,
    root: &std::path::Path,
    source_id: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<Buckets> {
    let elapsed_ms = parsed.scan.scan_elapsed.map(|d| d.as_millis() as u64);
    tracing::info!(
        source = source_id,
        scan_elapsed_ms = elapsed_ms,
        conversations_to_render = parsed
            .scan
            .render
            .as_ref()
            .map(|s| s.len() as i64)
            .unwrap_or(-1),
        cold_start = parsed.scan.render.is_none(),
        "[render] chatgpt dolt_diff scan"
    );

    let mut chats: Vec<NormalizedChat> = Vec::with_capacity(parsed.conversations.len());
    let mut blobs_by_chat: HashMap<String, BlobBundle> = HashMap::new();
    for c in &parsed.conversations {
        let shredded = shred(c);
        let mut chat = build_chat(&shredded, parsed, source_id);
        chat.inputs = c.inputs.declared();
        blobs_by_chat.insert(chat.id.clone(), c.blobs.clone());
        chats.push(chat);
    }

    let summary = cc_render_all(
        &profile(),
        &chats,
        root,
        source_id,
        &blobs_by_chat,
        progress,
        on_doc_complete,
    )
    .context("chatgpt chat-common render")?;
    Ok(summary.buckets)
}

/// One [`NormalizedChat`] per conversation. Messages are ordered by the
/// `current_node → root` parent walk (falling back to a `create_time`
/// sort), one [`NormalizedChatItem`] each.
fn build_chat(
    shredded: &ShreddedConversation,
    parsed: &ParsedChatGPTApi,
    source_id: &str,
) -> NormalizedChat {
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
    for (m, branch) in &path {
        // Own stamp, else the previous item's + 1ms (§6's sanctioned
        // inheritance), else nothing: a conversation with no
        // `create_time` of its own whose messages carry none either
        // genuinely has no time to report, and `None` says so rather
        // than filing the whole thread under 1970.
        let mut problems = Vec::new();
        let ms = own_stamp_ms(
            m.create_time.as_deref(),
            "create_time",
            iso_to_ms,
            &mut problems,
        )
        .or_else(|| last_ms.map(|p| p + 1));
        last_ms = ms.or(last_ms);

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
        let body = with_sources(render_message_body(&parts), &m.raw_json);
        problems.extend(uncovered_parts(&parts));

        let attachments: Vec<NormalizedAttachment> =
            m.attachments.iter().map(att_to_norm).collect();
        // An empty thought or a browsing step that showed nothing.
        if body.is_none() && attachments.is_empty() && problems.is_empty() {
            continue;
        }
        let is_aside = is_aside(m.role.as_deref(), !attachments.is_empty());
        let kind = if attachments.is_empty() {
            ItemKind::Text
        } else {
            ItemKind::Attachment
        };

        let msg_id = ids::message(source_id, &conv_id, &m.message_id, ms);
        items.push(NormalizedChatItem {
            message_uuid: msg_id.uuid.clone(),
            author_handle: None,
            author_display,
            date_ms: ms,
            text: body,
            kind,
            attachments,
            reactions: Vec::new(),
            labels: Vec::new(),
            system_note: None,
            source_url: None,
            kind_label: Some(kind_label.to_string()),
            source_ref: Some(UpstreamRef::new(
                msg_id.entity_kind,
                msg_id.natural_key.clone(),
            )),
            is_aside,
            branch: branch.clone(),
            unread: false,
            recipients: Vec::new(),
            mentions: Vec::new(),
            problems,
        });
    }

    let title = conv
        .title
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(untitled)".to_string());
    let chat_uuid = ids::conversation(source_id, &conv_id).uuid;
    NormalizedChat {
        contacts: Vec::new(),
        inputs: Vec::new(),
        path_prefix: None,
        id: chat_uuid.clone(),
        chat_uuid: chat_uuid.clone(),
        display: title.clone(),
        title: Some(title),
        author: None,
        account: conv
            .account_id
            .as_deref()
            .and_then(|id| parsed.account_label(id)),
        project: None,
        // ChatGPT's own conversation id — the round-trip route back
        // to chatgpt.com now that `uuid` is a minted v5.
        external_id: Some(conv_id.clone()),
        source_url: Some(format!("https://chatgpt.com/c/{conv_id}")),
        upstream_account: None,
        org_uuid: None,
        org_name: None,
        buckets: vec![NormalizedDoc {
            orphan_reactions: Vec::new(),
            period_key: "all".to_string(),
            markdown_uuid: chat_uuid,
            source_ref: None,
            items,
        }],
    }
}

/// Every message in reading order with the branches it sits in: the
/// branch ending at `current_node`, the one last seen, with every other
/// version folded in where it forked. With no usable `current_node`,
/// everything by `create_time` on one branch.
fn ordered_messages(shredded: &ShreddedConversation) -> Vec<(&OAMessageRow, Vec<String>)> {
    let mut by_time: Vec<&OAMessageRow> = shredded.messages.iter().collect();
    by_time.sort_by(|a, b| {
        a.create_time
            .as_deref()
            .unwrap_or("")
            .cmp(b.create_time.as_deref().unwrap_or(""))
    });
    let nodes: Vec<TreeNode<'_>> = by_time
        .iter()
        .map(|m| TreeNode {
            id: &m.message_id,
            parent: m.parent_id.as_deref(),
        })
        .collect();
    let order = shredded
        .conv
        .current_node
        .as_deref()
        .and_then(|leaf| reading_order(&nodes, leaf));
    let Some(order) = order else {
        return by_time.into_iter().map(|m| (m, Vec::new())).collect();
    };
    let by_id: HashMap<&str, &OAMessageRow> = by_time
        .iter()
        .map(|m| (m.message_id.as_str(), *m))
        .collect();
    order
        .into_iter()
        .map(|p| {
            let branch = p.branch.iter().map(|b| b.to_string()).collect();
            (by_id[p.id], branch)
        })
        .collect()
}

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
            // Code the interpreter ran comes labelled `unknown`.
            "code" => blocks.push(md_code_block(
                p.language
                    .as_deref()
                    .filter(|l| *l != "unknown")
                    .unwrap_or(""),
                t,
            )),
            "execution_output" => blocks.push(md_code_block("", t)),
            "thoughts" | "reasoning_recap" => blocks.push(format!("> {}", t.replace('\n', "\n> "))),
            "tether_quote" => {
                let title = p
                    .raw_json
                    .get("title")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                // A file's own words, not the model's markdown.
                let quote = format!("> {}", escape_md_block(t).replace('\n', "\n> "));
                blocks.push(if title.is_empty() {
                    quote
                } else {
                    format!("**{}**\n\n{quote}", escape_md_inline(title))
                });
            }
            _ => blocks.push(t.to_string()),
        }
    }
    let body = blocks.join("\n\n");
    (!body.trim().is_empty()).then_some(body)
}

/// The pages a message's citations point at, listed after its text:
/// the `cite` markers in the text are dropped, and these are where
/// their urls live.
fn with_sources(body: Option<String>, message: &Value) -> Option<String> {
    let refs = message
        .pointer("/metadata/content_references")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    let list = sources_list(refs.iter().flat_map(cited_pages));
    match (body, list) {
        (Some(body), Some(list)) => Some(format!("{body}\n\n{list}")),
        (body, list) => body.or(list),
    }
}

/// A reference's pages: its own url, a `grouped_webpages` group's items
/// and the sites supporting them, a `sources_footnote`'s sources.
fn cited_pages(reference: &Value) -> Vec<Source<'_>> {
    let mut out: Vec<Source<'_>> = cited_page(reference).into_iter().collect();
    for item in array_at(reference, "items") {
        out.extend(cited_page(item));
        out.extend(
            array_at(item, "supporting_websites")
                .iter()
                .filter_map(cited_page),
        );
    }
    out.extend(array_at(reference, "sources").iter().filter_map(cited_page));
    out
}

fn cited_page(v: &Value) -> Option<Source<'_>> {
    let str_at = |k: &str| v.get(k).and_then(Value::as_str).filter(|s| !s.is_empty());
    Some(Source {
        url: str_at("url")?,
        title: str_at("title"),
        site: str_at("attribution"),
    })
}

fn array_at<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key)
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default()
}

/// A content type parse has no reading for: the message renders
/// without it, and says so.
fn uncovered_parts(parts: &[&OAContentPartRow]) -> Vec<Problem> {
    parts
        .iter()
        .filter(|p| p.text.is_none())
        .map(|p| Problem::field("content", Reason::UncoveredType, &p.kind))
        .collect()
}

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

/// Tool traffic folds away; a tool message carrying a file (a generated
/// image) is what was asked for, and stays in the reading flow.
fn is_aside(role: Option<&str>, carries_a_file: bool) -> bool {
    is_tool_role(role) && !carries_a_file
}

/// Whether a message is tool traffic, and so belongs in a collapsed
/// aside rather than in the reading flow.
///
/// Read off the role, *not* off `kind_for_role_and_type` below: that
/// function lumps `system` in with the tool roles under one "Tool Call"
/// label, and a system prompt is content someone may well want to read.
fn is_tool_role(role: Option<&str>) -> bool {
    matches!(
        role.unwrap_or("").to_ascii_lowercase().as_str(),
        "tool" | "function"
    )
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

#[cfg(test)]
mod tests {
    use super::*;

    fn part(kind: &str, language: Option<&str>, text: &str) -> OAContentPartRow {
        OAContentPartRow {
            message_id: "m1".to_string(),
            part_index: 0,
            kind: kind.to_string(),
            language: language.map(str::to_string),
            text: Some(text.to_string()),
            raw_json: serde_json::Value::Null,
        }
    }

    /// Code and its output are shown as they are, whatever they contain:
    /// a fence inside cannot close ours and let the rest out as markup.
    #[test]
    fn code_and_its_output_stay_inside_their_fences() {
        let code = part(
            "code",
            Some("python\n<b>"),
            "print('```')\n<script>x</script>",
        );
        let output = part("execution_output", None, "```\n<script>x</script> & co");
        let body = render_message_body(&[&code, &output]).unwrap();
        assert_eq!(
            body,
            "````python\nprint('```')\n<script>x</script>\n````\n\n\
             ````\n```\n<script>x</script> & co\n````"
        );
    }

    #[test]
    fn code_labelled_unknown_gets_a_bare_fence() {
        let code = part("code", Some("unknown"), "print(1)");
        assert_eq!(render_message_body(&[&code]).unwrap(), "```\nprint(1)\n```");
    }

    #[test]
    fn a_file_quote_is_a_titled_blockquote() {
        let mut quote = part("tether_quote", None, "Stardate 41153.7\nAll is well.");
        quote.raw_json = serde_json::json!({"title": "Captains *Log*.pdf"});
        assert_eq!(
            render_message_body(&[&quote]).unwrap(),
            "**Captains \\*Log\\*.pdf**\n\n> Stardate 41153.7\n> All is well."
        );
    }

    /// A generated image arrives as a tool message, and was folded away
    /// with the plumbing.
    #[test]
    fn a_tool_message_with_a_file_stays_in_the_flow() {
        assert!(is_aside(Some("tool"), false));
        assert!(!is_aside(Some("tool"), true));
        assert!(!is_aside(Some("system"), false));
    }

    /// ChatGPT's `cite` markers are dropped from the text; the pages
    /// they point at were dropped with them.
    #[test]
    fn cited_pages_are_listed_after_the_text() {
        let message = serde_json::json!({"metadata": {"content_references": [
            {"type": "grouped_webpages", "items": [{
                "url": "https://memory-alpha.example/Risa", "title": "Risa",
                "attribution": "Memory Alpha",
                "supporting_websites": [{"url": "https://example.com/risa", "title": "Visit Risa"}]
            }]},
            {"type": "sources_footnote", "sources": [
                {"url": "https://memory-alpha.example/Risa", "title": "Risa"}
            ]},
            {"type": "entity", "name": "Risa"}
        ]}});
        assert_eq!(
            with_sources(Some("Risa is warm.".into()), &message).unwrap(),
            "Risa is warm.\n\n**Sources**\n\n\
             1. [Risa](https://memory-alpha.example/Risa) — Memory Alpha\n\
             2. [Visit Risa](https://example.com/risa)"
        );
        assert_eq!(
            with_sources(Some("x".into()), &serde_json::json!({})).unwrap(),
            "x"
        );
    }

    /// A content type parse cannot read is a problem on its message, not
    /// a silently empty body.
    #[test]
    fn an_unread_content_type_is_reported() {
        let mut unknown = part("holo_program", None, "");
        unknown.text = None;
        let text = part("text", None, "Computer, end program.");
        let problems = uncovered_parts(&[&unknown, &text]);
        assert_eq!(problems.len(), 1);
        assert_eq!(problems[0].reason, Reason::UncoveredType);
        assert_eq!(problems[0].sample, "holo_program");
    }
}
