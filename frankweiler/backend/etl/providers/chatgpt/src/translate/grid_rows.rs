//! Port of the `_openai_*` helpers + per-conversation row emit loop from
//! `src/ingest/grid_rows.py`. Produces one `GridRow` per ChatGPT
//! conversation (Chat row) plus one row per surfaced message; the
//! translator pairs these with the rendered `.md` in a
//! `.grid_rows.json` sidecar consumed by the provider-agnostic Load
//! step.

use std::hash::{Hash, Hasher};

use frankweiler_schema::grid_rows::GridRow;
use serde_json::Value;

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

/// Stable hash over the conversation's full upstream payload (the JSON
/// returned by the ChatGPT backend, with its `mapping` of messages +
/// content parts intact). Canonicalized (sorted keys) so cosmetic JSON
/// reordering doesn't invalidate the fingerprint.
///
/// One conversation = one document = one fingerprint, computed without
/// shredding the mapping. Renderer skips against this before deciding
/// whether to walk the mapping at all.
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
    // used to walk all global messages + content_parts for each
    // conversation. The new flow fingerprints the upstream payload of
    // one conversation at a time, so a loop over C conversations is
    // O(total bytes), not O(C × (M + P)).
    //
    // At C=400 conversations × M=40 messages, the buggy code would
    // take tens of seconds in fastbuild (debug, which Bazel runs);
    // the fix runs in well under a second. 5 s gives clean separation
    // in both build modes without flaking on busy CI — wall-clock
    // perf tests are coarse by nature.
    #[test]
    fn fingerprint_loop_is_linear_in_conversations() {
        const C: usize = 400;
        const M: usize = 40;
        let mut payloads: Vec<Value> = Vec::with_capacity(C);
        for ci in 0..C {
            let mut mapping = serde_json::Map::new();
            for mi in 0..M {
                let node_id = format!("n-{ci:04}-{mi:04}");
                mapping.insert(
                    node_id.clone(),
                    json!({
                        "id": node_id,
                        "message": {
                            "id": format!("m-{ci:04}-{mi:04}"),
                            "author": {"role": "user"},
                            "content": {"content_type": "text", "parts": [format!("hi {mi}")]},
                            "create_time": 1_700_000_000.0 + mi as f64,
                        },
                        "parent": null,
                    }),
                );
            }
            payloads.push(json!({
                "conversation_id": format!("c-{ci:04}"),
                "title": format!("conv {ci}"),
                "mapping": Value::Object(mapping),
            }));
        }

        let start = Instant::now();
        let mut h: u64 = 0;
        for p in &payloads {
            let fp = fingerprint_for_conversation(p);
            // Touch the result so the optimizer can't drop the call.
            h ^= fp.bytes().next().unwrap_or(0) as u64;
        }
        let elapsed = start.elapsed();
        assert!(h != u64::MAX);
        assert!(
            elapsed < Duration::from_secs(5),
            "fingerprint loop took {elapsed:?} for {C} conversations × {M} msgs — likely regressed to O(N²)",
        );
    }
}
