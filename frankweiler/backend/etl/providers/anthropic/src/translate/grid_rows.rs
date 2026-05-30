//! Port of the `_anthropic_*` helpers + per-conversation row emit loop
//! from `src/ingest/grid_rows.py`. Produces a Chat row, one row per
//! message, plus a row per `thinking` / `tool_use` / `tool_result`
//! block — same row set the Python ingest emits.

use std::hash::{Hash, Hasher};

use chrono::{DateTime, FixedOffset};
use frankweiler_schema::grid_rows::GridRow;
use serde_json::Value;

use super::parse::{ContentBlockRow, ConversationRow, MessageRow, ShreddedConversation};
use super::render::section_uuid_for_block;

// Bumped from 1 to 2 when the section-uuid scheme replaced
// `{msg_uuid}:{block_index}` for tool_use / tool_result / thinking
// rows. Old sidecars need a rebake to pick up the new uuids; bumping
// the version is the trigger for that.
pub const RENDER_VERSION: u32 = 2;

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

fn qmd_path(account_uuid: &str, conv_uuid: &str) -> String {
    // Page-dir layout: `<conv_uuid>/index.md`. Matches
    // `Rendered::relative_path` in `render.rs`.
    format!("rendered_md/anthropic/{account_uuid}/llm_chats/{conv_uuid}/index.md")
}

pub fn rows_for_conversation(shredded: &ShreddedConversation) -> Vec<GridRow> {
    let conv = &shredded.conv;
    let conv_uuid = conv.conversation_uuid.as_str();
    let model = conv
        .raw_json
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();

    let mut rows = Vec::new();
    rows.push(chat_row(conv));

    let mut msgs: Vec<&MessageRow> = shredded.messages.iter().collect();
    msgs.sort_by(|a, b| {
        let ka = (
            a.created_at.as_deref().unwrap_or(""),
            a.message_uuid.as_str(),
        );
        let kb = (
            b.created_at.as_deref().unwrap_or(""),
            b.message_uuid.as_str(),
        );
        ka.cmp(&kb)
    });

    let mut blocks_by_msg: std::collections::HashMap<&str, Vec<&ContentBlockRow>> =
        std::collections::HashMap::new();
    for b in &shredded.content_blocks {
        blocks_by_msg.entry(&b.message_uuid).or_default().push(b);
    }

    for (msg_idx, m) in msgs.iter().enumerate() {
        let kind = kind_for_sender(m.sender.as_deref().unwrap_or(""));
        let author = match kind {
            "User Input" => Some(conv.account_uuid.clone()),
            "LLM Response" => Some(model.clone())
                .filter(|s| !s.is_empty())
                .or_else(|| m.sender.clone()),
            _ => m.sender.clone(),
        };

        let mut blocks: Vec<&ContentBlockRow> = blocks_by_msg
            .get(m.message_uuid.as_str())
            .cloned()
            .unwrap_or_default();
        blocks.sort_by_key(|b| b.block_index);

        let text_parts: Vec<&str> = blocks
            .iter()
            .filter(|b| b.r#type.as_deref() == Some("text"))
            .filter_map(|b| b.text.as_deref())
            .filter(|s| !s.is_empty())
            .collect();
        let row_text = if !text_parts.is_empty() {
            text_parts.join("\n\n")
        } else {
            m.text.clone().unwrap_or_default()
        };

        rows.push(GridRow {
            uuid: m.message_uuid.clone(),
            provider: "anthropic".into(),
            kind: kind.into(),
            source_label: "Claude".into(),
            when_ts: m.created_at.clone().unwrap_or_default(),
            author,
            account: Some(conv.account_uuid.clone()),
            project: conv.project_uuid.clone(),
            channel: None,
            conversation_name: conv.name.clone(),
            conversation_uuid: conv_uuid.into(),
            message_index: Some(msg_idx as i64),
            entire_chat: format!("/chat/{conv_uuid}"),
            text: row_text,
            slack_link: None,
            qmd_path: Some(qmd_path(&conv.account_uuid, conv_uuid)),
            source_url: None,
            git_sha: None,
            external_id: None,
            notion_page_uuid: None,
            notion_block_uuid: None,
            document_uuid: Some(conv_uuid.into()),
        });

        for b in blocks {
            let btype = b.r#type.as_deref().unwrap_or("");
            if !matches!(btype, "tool_use" | "tool_result" | "thinking") {
                continue;
            }
            let mut btext = b.text.clone().unwrap_or_default();
            if btext.is_empty() && btype == "thinking" {
                if let Some(t) = b.raw_json.get("thinking").and_then(Value::as_str) {
                    btext = t.into();
                }
            }
            let row_when = b
                .start_timestamp
                .clone()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| {
                    bump_micros(
                        m.created_at.as_deref().unwrap_or(""),
                        (b.block_index + 1) as i64,
                    )
                });
            let row_author = if !model.is_empty() {
                model.clone()
            } else {
                btype.to_string()
            };
            let raw_obj = b.raw_json.as_object().cloned().unwrap_or_default();
            let row_uuid =
                section_uuid_for_block(&m.message_uuid, b.block_index, Some(btype), &raw_obj)
                    // section_uuid_for_block returns None only for `text`,
                    // which the outer `matches!` above already excluded; the
                    // synthetic fallback is purely defensive.
                    .unwrap_or_else(|| format!("blk-{}-{}", m.message_uuid, b.block_index));
            rows.push(GridRow {
                uuid: row_uuid,
                provider: "anthropic".into(),
                kind: kind_for_block(btype).into(),
                source_label: "Claude".into(),
                when_ts: row_when,
                author: Some(row_author),
                account: Some(conv.account_uuid.clone()),
                project: conv.project_uuid.clone(),
                channel: None,
                conversation_name: conv.name.clone(),
                conversation_uuid: conv_uuid.into(),
                message_index: Some(msg_idx as i64),
                entire_chat: format!("/chat/{conv_uuid}"),
                text: if btext.is_empty() {
                    btype.into()
                } else {
                    btext
                },
                slack_link: None,
                qmd_path: Some(qmd_path(&conv.account_uuid, conv_uuid)),
                source_url: None,
                git_sha: None,
                external_id: None,
                notion_page_uuid: None,
                notion_block_uuid: None,
                document_uuid: Some(conv_uuid.into()),
            });
        }
    }
    rows
}

fn chat_row(conv: &ConversationRow) -> GridRow {
    let when = conv
        .created_at
        .clone()
        .or_else(|| conv.updated_at.clone())
        .unwrap_or_default();
    let text = conv
        .summary
        .clone()
        .filter(|s| !s.is_empty())
        .or_else(|| conv.name.clone())
        .unwrap_or_default();
    GridRow {
        uuid: conv.conversation_uuid.clone(),
        provider: "anthropic".into(),
        kind: "Chat".into(),
        source_label: "Claude".into(),
        when_ts: when,
        author: None,
        account: Some(conv.account_uuid.clone()),
        project: conv.project_uuid.clone(),
        channel: None,
        conversation_name: conv.name.clone(),
        conversation_uuid: conv.conversation_uuid.clone(),
        message_index: None,
        entire_chat: format!("/chat/{}", conv.conversation_uuid),
        text,
        slack_link: None,
        qmd_path: Some(qmd_path(&conv.account_uuid, &conv.conversation_uuid)),
        source_url: None,
        git_sha: None,
        external_id: None,
        notion_page_uuid: None,
        notion_block_uuid: None,
        document_uuid: Some(conv.conversation_uuid.clone()),
    }
}

/// Stable hash over the conversation's full upstream payload (the
/// normalized-to-export-shape JSON, with `chat_messages` intact).
/// Canonicalized (sorted keys) so cosmetic JSON reordering doesn't
/// invalidate the fingerprint.
///
/// One conversation = one document = one fingerprint, computed without
/// shredding `chat_messages`. Renderer skips against this before
/// deciding whether to walk the messages at all.
pub fn fingerprint_for_conversation(upstream_payload: &Value) -> String {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    RENDER_VERSION.hash(&mut h);
    canonical_json(upstream_payload).hash(&mut h);
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::time::{Duration, Instant};

    // Regression test for the quadratic fingerprint/render flow that
    // used to walk all global messages + content_blocks + attachments
    // for each conversation. The new flow fingerprints the upstream
    // payload of one conversation at a time.
    //
    // At C=400 conversations × M=40 messages each, the old code burned
    // a few hundred ms in release; the new code is ~10 ms. 500 ms
    // gives comfortable headroom.
    #[test]
    fn fingerprint_loop_is_linear_in_conversations() {
        const C: usize = 400;
        const M: usize = 40;
        let mut payloads: Vec<Value> = Vec::with_capacity(C);
        for ci in 0..C {
            let mut msgs = Vec::with_capacity(M);
            for mi in 0..M {
                msgs.push(json!({
                    "uuid": format!("m-{ci:04}-{mi:04}"),
                    "sender": if mi % 2 == 0 { "human" } else { "assistant" },
                    "text": format!("hi {mi}"),
                    "created_at": format!("2026-01-01T00:{mi:02}:00Z"),
                    "content": [{"type": "text", "text": format!("body {mi}")}],
                }));
            }
            payloads.push(json!({
                "uuid": format!("c-{ci:04}"),
                "name": format!("conv {ci}"),
                "account": {"uuid": "acct-1"},
                "chat_messages": msgs,
            }));
        }

        let start = Instant::now();
        let mut h: u64 = 0;
        for p in &payloads {
            let fp = fingerprint_for_conversation(p);
            h ^= fp.bytes().next().unwrap_or(0) as u64;
        }
        let elapsed = start.elapsed();
        assert!(h != u64::MAX);
        assert!(
            elapsed < Duration::from_millis(500),
            "fingerprint loop took {elapsed:?} for {C} conversations × {M} msgs — likely regressed to O(N²)",
        );
    }
}
