//! Hermes render: normalize parsed sessions into the shared `chat-common`
//! model and delegate Markdown / grid-row / sidecar plumbing to
//! [`frankweiler_etl_chat_common::render::render_all`].
//!
//! One session → one [`NormalizedChat`] with a single `"all"` bucket. Each
//! message → one [`NormalizedChatItem`] whose `kind_label` carries the
//! role-distinguished grid kind ("User Input" / "LLM Response" / "LLM
//! Thinking" / "Tool Call" / "System"), matching how the ChatGPT provider
//! surfaces per-message roles. UUIDs are minted deterministically (UUIDv5 under
//! a fixed Hermes namespace) so raw ids like `s_001` still produce stable page
//! identities and snapshots.

use std::collections::HashMap;

use anyhow::{Context as _, Result};
use frankweiler_etl::blob_cas::BlobBundle;
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl_chat_common::render::{render_all as cc_render_all, RenderProfile};
use frankweiler_etl_chat_common::types::{
    ItemKind, NormalizedChat, NormalizedChatItem, NormalizedDoc,
};
use uuid::Uuid;

use super::parse::{HermesMessage, HermesSession, ParsedHermesExport};

/// Bump when the item-shape / column mapping changes meaningfully.
pub const RENDER_VERSION: u32 = 1;

/// Fixed namespace for Hermes v5 UUIDs (a random-but-stable UUID). Every
/// Hermes-minted id derives from this so identities are deterministic and
/// collision-free against other providers.
const HERMES_UUID_NS: Uuid = Uuid::from_bytes([
    0x48, 0x45, 0x52, 0x4d, 0x45, 0x53, 0x40, 0x00, 0x80, 0x00, 0x6e, 0x6f, 0x75, 0x73, 0x00, 0x01,
]);

fn profile() -> RenderProfile {
    RenderProfile {
        provider: "hermes",
        source_label: "Hermes".to_string(),
        chat_kind: "Chat".to_string(),
        // Per-message kind is always set via `kind_label`; this is a nominal
        // fallback.
        message_kind: "LLM Response".to_string(),
        reaction_kind: "Hermes Reaction".to_string(),
        render_version: RENDER_VERSION,
    }
}

fn chat_uuid(session_id: &str) -> String {
    Uuid::new_v5(&HERMES_UUID_NS, session_id.as_bytes()).to_string()
}

fn message_uuid(session_id: &str, index: usize) -> String {
    Uuid::new_v5(&HERMES_UUID_NS, format!("{session_id}#{index}").as_bytes()).to_string()
}

/// Render every session in `parsed` via the shared chat renderer.
pub fn render_all(
    parsed: &ParsedHermesExport,
    root: &std::path::Path,
    source_name: &str,
    progress: &Progress,
    prior_fingerprints: &HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<()> {
    let chats: Vec<NormalizedChat> = parsed.sessions.iter().map(build_chat).collect();

    // Hermes has no blob attachments in v1.
    let no_blobs: HashMap<String, BlobBundle> = HashMap::new();
    cc_render_all(
        &profile(),
        &chats,
        root,
        source_name,
        &no_blobs,
        progress,
        prior_fingerprints,
        on_doc_complete,
    )
    .context("hermes chat-common render")?;
    Ok(())
}

fn build_chat(session: &HermesSession) -> NormalizedChat {
    let sid = &session.id;
    let cu = chat_uuid(sid);

    // Timestamp bump: a message with no timestamp inherits the previous
    // effective time + 1ms so ordering stays stable (mirrors the ChatGPT
    // renderer).
    let mut last_ms = session.started_at_ms;
    let items: Vec<NormalizedChatItem> = session
        .messages
        .iter()
        .enumerate()
        .map(|(idx, m)| {
            let ms = m
                .timestamp_ms
                .or_else(|| last_ms.map(|p| p + 1))
                .unwrap_or(0);
            last_ms = Some(ms);
            build_item(session, m, idx, ms)
        })
        .collect();

    let title = session
        .title
        .clone()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(untitled)".to_string());

    NormalizedChat {
        id: sid.clone(),
        chat_uuid: cu.clone(),
        display: title.clone(),
        title: Some(title),
        // The Hermes user id owns the account column; the surface
        // (cli/telegram/…) is the sub-group context.
        account: session.user_id.clone(),
        project: session.source.clone(),
        // Preserve the parent/child edge as the upstream id for now (first-class
        // edge rows are future work).
        external_id: session.parent_session_id.clone(),
        source_url: None,
        org_uuid: None,
        org_name: None,
        buckets: vec![NormalizedDoc {
            period_key: "all".to_string(),
            markdown_uuid: cu,
            items,
        }],
    }
}

fn build_item(
    session: &HermesSession,
    m: &HermesMessage,
    idx: usize,
    ms: i64,
) -> NormalizedChatItem {
    let role = m.role.to_ascii_lowercase();
    let has_content = m.content.as_deref().is_some_and(|s| !s.is_empty());
    let has_reasoning = m.reasoning.as_deref().is_some_and(|s| !s.is_empty());
    let kind_label = kind_for_role(&role, has_content, has_reasoning);

    let author_display = match kind_label {
        "User Input" => "User".to_string(),
        "LLM Response" | "LLM Thinking" => m
            .model
            .clone()
            .or_else(|| session.model.clone())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "Assistant".to_string()),
        "Tool Call" => m
            .tool_name
            .clone()
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "tool".to_string()),
        _ => capitalize(&role),
    };

    let is_system = kind_label == "System";
    let item_kind = if is_system {
        ItemKind::System
    } else {
        ItemKind::Text
    };

    NormalizedChatItem {
        message_uuid: message_uuid(&session.id, idx),
        author_id: role.clone(),
        author_display,
        date_ms: ms,
        text: build_body(m),
        kind: item_kind,
        attachments: Vec::new(),
        reactions: Vec::new(),
        system_note: if is_system {
            Some("system message".to_string())
        } else {
            None
        },
        source_url: None,
        kind_label: Some(kind_label.to_string()),
    }
}

/// Compose a message body from content + reasoning (blockquote) + tool_calls
/// (fenced JSON). Returns `None` when there's nothing to show.
fn build_body(m: &HermesMessage) -> Option<String> {
    let mut blocks: Vec<String> = Vec::new();
    if let Some(text) = m.content.as_deref().filter(|s| !s.is_empty()) {
        blocks.push(text.to_string());
    }
    if let Some(reasoning) = m.reasoning.as_deref().filter(|s| !s.is_empty()) {
        blocks.push(format!("> {}", reasoning.replace('\n', "\n> ")));
    }
    if let Some(calls) = m.tool_calls_pretty.as_deref().filter(|s| !s.is_empty()) {
        blocks.push(format!("**Tool calls:**\n\n```json\n{calls}\n```"));
    }
    let body = blocks.join("\n\n");
    (!body.trim().is_empty()).then_some(body)
}

fn kind_for_role(role: &str, has_content: bool, has_reasoning: bool) -> &'static str {
    match role {
        "user" => "User Input",
        "assistant" => {
            if !has_content && has_reasoning {
                "LLM Thinking"
            } else {
                "LLM Response"
            }
        }
        "tool" => "Tool Call",
        "system" => "System",
        _ => "Tool Call",
    }
}

fn capitalize(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(c) => {
            let mut out: String = c.to_uppercase().collect();
            out.extend(chars);
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::render_and_index_md::parse::HermesMessage;

    fn msg(role: &str, content: Option<&str>) -> HermesMessage {
        HermesMessage {
            role: role.to_string(),
            content: content.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn uuid_is_stable_and_v5() {
        assert_eq!(chat_uuid("s_001"), chat_uuid("s_001"));
        assert_ne!(chat_uuid("s_001"), chat_uuid("s_002"));
        assert_eq!(message_uuid("s_001", 0), message_uuid("s_001", 0));
        assert_ne!(message_uuid("s_001", 0), message_uuid("s_001", 1));
    }

    #[test]
    fn kind_mapping() {
        assert_eq!(kind_for_role("user", true, false), "User Input");
        assert_eq!(kind_for_role("assistant", true, false), "LLM Response");
        assert_eq!(kind_for_role("assistant", false, true), "LLM Thinking");
        assert_eq!(kind_for_role("tool", true, false), "Tool Call");
        assert_eq!(kind_for_role("system", true, false), "System");
    }

    #[test]
    fn body_includes_reasoning_and_tool_calls() {
        let m = HermesMessage {
            role: "assistant".to_string(),
            content: Some("Working on it".to_string()),
            reasoning: Some("think\nmore".to_string()),
            tool_calls_pretty: Some("{\n  \"name\": \"terminal\"\n}".to_string()),
            ..Default::default()
        };
        let body = build_body(&m).unwrap();
        assert!(body.contains("Working on it"));
        assert!(body.contains("> think\n> more"));
        assert!(body.contains("```json"));
    }

    #[test]
    fn tool_item_author_is_tool_name() {
        let session = HermesSession {
            id: "s".to_string(),
            ..Default::default()
        };
        let mut m = msg("tool", Some("stdout..."));
        m.tool_name = Some("terminal".to_string());
        let item = build_item(&session, &m, 0, 1000);
        assert_eq!(item.author_display, "terminal");
        assert_eq!(item.kind_label.as_deref(), Some("Tool Call"));
    }

    #[test]
    fn assistant_author_falls_back_to_session_model() {
        let session = HermesSession {
            id: "s".to_string(),
            model: Some("anthropic/claude-sonnet-4".to_string()),
            ..Default::default()
        };
        let item = build_item(&session, &msg("assistant", Some("hi")), 0, 1000);
        assert_eq!(item.author_display, "anthropic/claude-sonnet-4");
    }
}
