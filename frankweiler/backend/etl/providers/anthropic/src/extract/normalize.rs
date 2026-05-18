//! Port of `_normalize_to_export_shape` + `_synthesize_message_text`
//! from `src/download/claude_web.py`. The live API:
//!   - omits `account` (we synthesize from a known account_uuid)
//!   - leaves `message.text` empty and puts prose in `content[].text`
//!   - drops `flags` from content blocks (export has `flags: null`)
//!
//! We're only generous enough to make the parser happy; we leave
//! every other upstream field alone.

use serde_json::{json, Map, Value};

pub fn normalize_to_export_shape(
    mut api_conv: Value,
    account_uuid: Option<&str>,
    org_uuid: &str,
) -> Value {
    let Some(obj) = api_conv.as_object_mut() else {
        return api_conv;
    };
    if let Some(acct) = account_uuid {
        obj.entry("account")
            .or_insert_with(|| json!({ "uuid": acct }));
    }
    if let Some(msgs) = obj.get_mut("chat_messages").and_then(|v| v.as_array_mut()) {
        for m in msgs.iter_mut() {
            let needs_text = m
                .get("text")
                .map(|t| matches!(t, Value::Null) || matches!(t, Value::String(s) if s.is_empty()))
                .unwrap_or(true);
            if needs_text {
                let synthesized = synthesize_message_text(
                    m.get("content")
                        .and_then(|v| v.as_array())
                        .unwrap_or(&Vec::new()),
                );
                if let Some(mobj) = m.as_object_mut() {
                    mobj.insert("text".into(), Value::String(synthesized));
                }
            }
            if let Some(blocks) = m.get_mut("content").and_then(|v| v.as_array_mut()) {
                for b in blocks.iter_mut() {
                    if let Some(bobj) = b.as_object_mut() {
                        bobj.entry("flags").or_insert(Value::Null);
                    }
                }
            }
        }
    }
    let mut source = Map::new();
    source.insert("via".into(), Value::String("claude.ai/api".into()));
    source.insert("org_uuid".into(), Value::String(org_uuid.into()));
    obj.insert("_source".into(), Value::Object(source));
    api_conv
}

/// Recreate the export's top-level `message.text` by joining each
/// content block's prose: `text` blocks → `text`, `thinking` blocks →
/// `thinking`. The export inserts placeholders around redacted
/// thinking that we cannot reproduce; this preserves the prose the
/// API actually returns.
pub fn synthesize_message_text(blocks: &[Value]) -> String {
    let mut parts = String::new();
    for b in blocks {
        let t = b.get("type").and_then(|v| v.as_str());
        match t {
            Some("text") => {
                if let Some(s) = b.get("text").and_then(|v| v.as_str()) {
                    parts.push_str(s);
                }
            }
            Some("thinking") => {
                if let Some(s) = b.get("thinking").and_then(|v| v.as_str()) {
                    parts.push_str(s);
                }
            }
            _ => {}
        }
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthesize_joins_text_and_thinking() {
        let blocks = vec![
            json!({"type": "text", "text": "hi"}),
            json!({"type": "thinking", "thinking": " (ponder)"}),
            json!({"type": "tool_use", "name": "search"}),
            json!({"type": "text", "text": " bye"}),
        ];
        assert_eq!(synthesize_message_text(&blocks), "hi (ponder) bye");
    }

    #[test]
    fn normalize_fills_account_and_flags() {
        let api = json!({
            "chat_messages": [
                {
                    "content": [
                        {"type": "text", "text": "hello"},
                    ],
                }
            ]
        });
        let out = normalize_to_export_shape(api, Some("acct-123"), "org-abc");
        assert_eq!(out["account"]["uuid"], "acct-123");
        assert_eq!(out["chat_messages"][0]["text"], "hello");
        assert_eq!(out["chat_messages"][0]["content"][0]["flags"], Value::Null);
        assert_eq!(out["_source"]["org_uuid"], "org-abc");
        assert_eq!(out["_source"]["via"], "claude.ai/api");
    }
}
