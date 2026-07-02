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

/// Mint a stable message UUID. When the upstream message carries an id
/// (`id`/`message_id`/`uuid`), derive from `session_id + upstream id` so the
/// identity is durable. Otherwise fall back to a deterministic key built from
/// the timestamp, role, and rendered body — not the positional index — so
/// inserting or dropping a sibling message doesn't renumber every later
/// message's identity.
fn message_uuid(session_id: &str, m: &HermesMessage, ms: i64, dup: usize) -> String {
    let base = identity_key(session_id, m, ms);
    // Disambiguate duplicate identity keys within a session. This protects both
    // legacy messages with no upstream ids and real stores that can contain the
    // same upstream id more than once after rewinds/branching. First occurrence
    // keeps the bare key; later ones append `#dup={n}`.
    let key = if dup == 0 {
        base
    } else {
        format!("{base}#dup={dup}")
    };
    Uuid::new_v5(&HERMES_UUID_NS, key.as_bytes()).to_string()
}

fn identity_key(session_id: &str, m: &HermesMessage, ms: i64) -> String {
    match m.id.as_deref().filter(|s| !s.is_empty()) {
        Some(id) => format!("{session_id}#msg:{id}"),
        None => fallback_key(session_id, m, ms),
    }
}

/// Deterministic fallback identity for a message with no upstream id: built
/// from `session_id + timestamp + role + rendered body` (not the positional
/// index) so inserting or dropping a sibling doesn't renumber later messages.
fn fallback_key(session_id: &str, m: &HermesMessage, ms: i64) -> String {
    format!(
        "{session_id}#ts={ms}#role={}#{}",
        m.role.to_ascii_lowercase(),
        build_body(m).unwrap_or_default()
    )
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
    // Count occurrences of each message identity key so duplicate legacy rows
    // or duplicate upstream ids mint distinct UUIDs instead of colliding on the
    // grid_rows.uuid UNIQUE constraint.
    let mut identity_counts: HashMap<String, usize> = HashMap::new();
    let items: Vec<NormalizedChatItem> = session
        .messages
        .iter()
        .map(|m| {
            let ms = m
                .timestamp_ms
                .or_else(|| last_ms.map(|p| p + 1))
                .unwrap_or(0);
            last_ms = Some(ms);
            let counter = identity_counts.entry(identity_key(sid, m, ms)).or_insert(0);
            let dup = *counter;
            *counter += 1;
            build_item(session, m, ms, dup)
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
    ms: i64,
    dup: usize,
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
        message_uuid: message_uuid(&session.id, m, ms, dup),
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

        // Upstream id → stable and independent of timestamp/position.
        let m_a = HermesMessage {
            id: Some("m_a".to_string()),
            ..msg("user", Some("hi"))
        };
        assert_eq!(
            message_uuid("s_001", &m_a, 1000, 0),
            message_uuid("s_001", &m_a, 9999, 0),
            "upstream-id UUID must not depend on timestamp"
        );
        let m_b = HermesMessage {
            id: Some("m_b".to_string()),
            ..msg("user", Some("hi"))
        };
        assert_ne!(
            message_uuid("s_001", &m_a, 1000, 0),
            message_uuid("s_001", &m_b, 1000, 0)
        );

        // No upstream id → deterministic content/timestamp fallback (not index).
        let no_id = msg("user", Some("hello"));
        assert_eq!(
            message_uuid("s_001", &no_id, 1000, 0),
            message_uuid("s_001", &no_id, 1000, 0)
        );
        assert_ne!(
            message_uuid("s_001", &no_id, 1000, 0),
            message_uuid("s_001", &no_id, 2000, 0),
            "fallback distinguishes by timestamp"
        );
        assert_ne!(
            message_uuid("s_001", &msg("user", Some("a")), 1000, 0),
            message_uuid("s_001", &msg("user", Some("b")), 1000, 0),
            "fallback distinguishes by content"
        );

        // Two identical fallback messages in one session (same ts/role/body, no
        // upstream id) get distinct UUIDs via the dup counter, avoiding the
        // grid_rows.uuid UNIQUE collision.
        assert_ne!(
            message_uuid("s_001", &no_id, 1000, 0),
            message_uuid("s_001", &no_id, 1000, 1),
            "duplicate fallback messages must mint distinct UUIDs"
        );
        // Duplicate upstream ids are rare but possible in real stores after
        // rewinds/branching; the dup counter disambiguates them too.
        assert_ne!(
            message_uuid("s_001", &m_a, 1000, 0),
            message_uuid("s_001", &m_a, 1000, 1),
            "duplicate upstream ids must mint distinct UUIDs"
        );
    }

    #[test]
    fn build_chat_disambiguates_duplicate_fallback_messages() {
        // Two legacy messages with no upstream id, identical timestamp, role,
        // and body — the exact collision that broke the grid_rows.uuid UNIQUE
        // constraint. build_chat must mint distinct UUIDs for them.
        let session = HermesSession {
            id: "s_dup".to_string(),
            started_at_ms: Some(1000),
            messages: vec![
                HermesMessage {
                    timestamp_ms: Some(1000),
                    ..msg("user", Some("same"))
                },
                HermesMessage {
                    timestamp_ms: Some(1000),
                    ..msg("user", Some("same"))
                },
                // A third, distinct-content message stays independent.
                HermesMessage {
                    timestamp_ms: Some(1000),
                    ..msg("user", Some("other"))
                },
            ],
            ..Default::default()
        };
        let chat = build_chat(&session);
        let uuids: Vec<&str> = chat.buckets[0]
            .items
            .iter()
            .map(|i| i.message_uuid.as_str())
            .collect();
        assert_eq!(uuids.len(), 3);
        assert_ne!(uuids[0], uuids[1], "duplicate fallback messages collided");
        assert_ne!(uuids[0], uuids[2]);
        assert_ne!(uuids[1], uuids[2]);
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
        let item = build_item(&session, &m, 1000, 0);
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
        let item = build_item(&session, &msg("assistant", Some("hi")), 1000, 0);
        assert_eq!(item.author_display, "anthropic/claude-sonnet-4");
    }
}
