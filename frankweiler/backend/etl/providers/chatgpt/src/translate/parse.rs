//! Port of `src/ingest/providers/openai/parse.py`. Reads a directory
//! laid out as `me.json`, `conversations.json`, `conversations/<id>.json`
//! (the wire shape that `chatgpt.com/backend-api` returns and that
//! `chatgpt_web.py` writes verbatim) and flattens it into typed rows.
//!
//! `raw_json` fields carry the JSON minus whatever has been exploded into
//! sibling row types — e.g. conversations drop `mapping`, messages drop
//! `content` — so the row payload stays bounded.

use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{Map, Value};

#[derive(Debug, Clone)]
pub struct OAAccountRow {
    pub account_id: String,
    pub email: Option<String>,
    pub name: Option<String>,
    pub raw_json: Value,
}

#[derive(Debug, Clone)]
pub struct OAConversationRow {
    pub account_id: Option<String>,
    pub conversation_id: String,
    pub title: Option<String>,
    pub create_time: Option<String>,
    pub update_time: Option<String>,
    pub current_node: Option<String>,
    pub default_model_slug: Option<String>,
    pub gizmo_id: Option<String>,
    pub gizmo_type: Option<String>,
    pub is_archived: Option<bool>,
    pub is_starred: Option<bool>,
    pub raw_json: Value,
}

#[derive(Debug, Clone)]
pub struct OAMessageRow {
    pub conversation_id: String,
    pub message_id: String,
    pub parent_id: Option<String>,
    pub role: Option<String>,
    pub recipient: Option<String>,
    pub channel: Option<String>,
    pub content_type: Option<String>,
    pub text: String,
    pub status: Option<String>,
    pub end_turn: Option<bool>,
    pub weight: Option<f64>,
    pub model_slug: Option<String>,
    pub create_time: Option<String>,
    pub update_time: Option<String>,
    pub raw_json: Value,
}

#[derive(Debug, Clone)]
pub struct OAContentPartRow {
    pub message_id: String,
    pub part_index: usize,
    pub kind: String,
    pub language: Option<String>,
    pub text: Option<String>,
    pub raw_json: Value,
}

#[derive(Debug, Default, Clone)]
pub struct ParsedChatGPTApi {
    pub accounts: Vec<OAAccountRow>,
    pub conversations: Vec<OAConversationRow>,
    pub messages: Vec<OAMessageRow>,
    pub content_parts: Vec<OAContentPartRow>,
}

/// Normalize a ChatGPT timestamp to an ISO-8601 string. Strings pass through
/// verbatim (preserving any embedded offset); numbers are rendered in UTC
/// with an explicit `+00:00` suffix. See the Python original for rationale.
fn epoch_to_iso(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) if s.is_empty() => None,
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => {
            let secs = n.as_f64()?;
            let micros = (secs * 1_000_000.0).round() as i64;
            let dt: DateTime<Utc> = DateTime::from_timestamp_micros(micros)?;
            // chrono's `%.6f` emits "+00:00" rather than "Z" — matches the
            // Python `isoformat(timespec="microseconds")` output exactly.
            Some(dt.format("%Y-%m-%dT%H:%M:%S%.6f+00:00").to_string())
        }
        _ => None,
    }
}

fn synthesize_text(content: Option<&Value>) -> String {
    let Some(content) = content.and_then(Value::as_object) else {
        return String::new();
    };
    let ct = content.get("content_type").and_then(Value::as_str);
    match ct {
        Some("text") => {
            let mut out: Vec<String> = Vec::new();
            if let Some(parts) = content.get("parts").and_then(Value::as_array) {
                for p in parts {
                    if let Some(s) = p.as_str() {
                        out.push(s.to_string());
                    } else if let Some(obj) = p.as_object() {
                        if let Some(t) = obj.get("text").and_then(Value::as_str) {
                            out.push(t.to_string());
                        }
                    }
                }
            }
            out.join("\n")
        }
        Some("code") | Some("execution_output") => content
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        Some("thoughts") => {
            let mut out: Vec<String> = Vec::new();
            if let Some(thoughts) = content.get("thoughts").and_then(Value::as_array) {
                for t in thoughts {
                    let Some(t) = t.as_object() else { continue };
                    if let Some(s) = t.get("summary") {
                        if !s.is_null() {
                            out.push(value_as_string_loose(s));
                        }
                    }
                    if let Some(b) = t.get("content") {
                        if !b.is_null() {
                            out.push(value_as_string_loose(b));
                        }
                    }
                }
            }
            out.join("\n\n")
        }
        Some("reasoning_recap") => content
            .get("content")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        Some("model_editable_context") => content
            .get("model_set_context")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => String::new(),
    }
}

fn value_as_string_loose(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn content_parts(message_id: &str, content: Option<&Value>) -> Vec<OAContentPartRow> {
    let mut rows = Vec::new();
    let Some(content) = content.and_then(Value::as_object) else {
        return rows;
    };
    let ct = content.get("content_type").and_then(Value::as_str);
    match ct {
        Some("text") => {
            if let Some(parts) = content.get("parts").and_then(Value::as_array) {
                for (i, p) in parts.iter().enumerate() {
                    if let Some(s) = p.as_str() {
                        let mut raw = Map::new();
                        raw.insert("content_type".into(), Value::from("text"));
                        raw.insert("value".into(), Value::from(s));
                        rows.push(OAContentPartRow {
                            message_id: message_id.into(),
                            part_index: i,
                            kind: "text".into(),
                            language: None,
                            text: Some(s.to_string()),
                            raw_json: Value::Object(raw),
                        });
                    } else {
                        let txt = p
                            .as_object()
                            .and_then(|o| o.get("text"))
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        let raw = if p.is_object() {
                            p.clone()
                        } else {
                            let mut m = Map::new();
                            m.insert("value".into(), p.clone());
                            Value::Object(m)
                        };
                        rows.push(OAContentPartRow {
                            message_id: message_id.into(),
                            part_index: i,
                            kind: "text".into(),
                            language: None,
                            text: Some(txt),
                            raw_json: raw,
                        });
                    }
                }
            }
        }
        Some("code") => {
            rows.push(OAContentPartRow {
                message_id: message_id.into(),
                part_index: 0,
                kind: "code".into(),
                language: content
                    .get("language")
                    .and_then(Value::as_str)
                    .map(String::from),
                text: Some(
                    content
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                ),
                raw_json: Value::Object(content.clone()),
            });
        }
        Some("execution_output") => {
            rows.push(OAContentPartRow {
                message_id: message_id.into(),
                part_index: 0,
                kind: "execution_output".into(),
                language: None,
                text: Some(
                    content
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                ),
                raw_json: Value::Object(content.clone()),
            });
        }
        Some("thoughts") => {
            if let Some(ts) = content.get("thoughts").and_then(Value::as_array) {
                for (i, t) in ts.iter().enumerate() {
                    let Some(obj) = t.as_object() else { continue };
                    let mut bits: Vec<String> = Vec::new();
                    for k in ["summary", "content"] {
                        if let Some(v) = obj.get(k) {
                            if !v.is_null() {
                                let s = value_as_string_loose(v);
                                if !s.is_empty() {
                                    bits.push(s);
                                }
                            }
                        }
                    }
                    rows.push(OAContentPartRow {
                        message_id: message_id.into(),
                        part_index: i,
                        kind: "thoughts".into(),
                        language: None,
                        text: Some(bits.join("\n\n")),
                        raw_json: t.clone(),
                    });
                }
            }
        }
        Some("reasoning_recap") => {
            let text = content
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            rows.push(OAContentPartRow {
                message_id: message_id.into(),
                part_index: 0,
                kind: "reasoning_recap".into(),
                language: None,
                text: Some(text),
                raw_json: Value::Object(content.clone()),
            });
        }
        Some("model_editable_context") => {
            rows.push(OAContentPartRow {
                message_id: message_id.into(),
                part_index: 0,
                kind: "model_editable_context".into(),
                language: None,
                text: Some(
                    content
                        .get("model_set_context")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                ),
                raw_json: Value::Object(content.clone()),
            });
        }
        other => {
            rows.push(OAContentPartRow {
                message_id: message_id.into(),
                part_index: 0,
                kind: other.unwrap_or("unknown").to_string(),
                language: None,
                text: None,
                raw_json: Value::Object(content.clone()),
            });
        }
    }
    rows
}

pub fn parse_api_dir(api_dir: &Path) -> Result<ParsedChatGPTApi> {
    let mut out = ParsedChatGPTApi::default();

    let me_path = api_dir.join("me.json");
    let mut account_id: Option<String> = None;
    if me_path.exists() {
        let me: Value = serde_json::from_str(&fs::read_to_string(&me_path)?)
            .with_context(|| format!("parsing {}", me_path.display()))?;
        if let Some(id) = me.get("id").and_then(Value::as_str) {
            account_id = Some(id.to_string());
            out.accounts.push(OAAccountRow {
                account_id: id.to_string(),
                email: me.get("email").and_then(Value::as_str).map(String::from),
                name: me.get("name").and_then(Value::as_str).map(String::from),
                raw_json: me,
            });
        }
    }

    let listing_path = api_dir.join("conversations.json");
    let mut listing_by_id: std::collections::HashMap<String, Value> =
        std::collections::HashMap::new();
    if listing_path.exists() {
        let v: Value = serde_json::from_str(&fs::read_to_string(&listing_path)?)
            .with_context(|| format!("parsing {}", listing_path.display()))?;
        if let Value::Array(items) = v {
            for item in items {
                if let Some(id) = item.get("id").and_then(Value::as_str) {
                    listing_by_id.insert(id.to_string(), item);
                }
            }
        }
    }

    let convs_dir = api_dir.join("conversations");
    if !convs_dir.is_dir() {
        return Ok(out);
    }
    let mut files: Vec<_> = fs::read_dir(&convs_dir)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("json"))
        .collect();
    files.sort();
    for f in files {
        let Ok(body) = fs::read_to_string(&f) else {
            continue;
        };
        let Ok(d): Result<Value, _> = serde_json::from_str(&body) else {
            continue;
        };
        let Some(d_obj) = d.as_object() else { continue };
        let cid = d_obj
            .get("conversation_id")
            .and_then(Value::as_str)
            .or_else(|| d_obj.get("id").and_then(Value::as_str))
            .map(String::from)
            .unwrap_or_else(|| {
                f.file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string()
            });

        let empty = Value::Object(Map::new());
        let listing_row = listing_by_id.get(&cid).unwrap_or(&empty);

        let create_time = epoch_to_iso(d_obj.get("create_time").unwrap_or(&Value::Null))
            .or_else(|| epoch_to_iso(listing_row.get("create_time").unwrap_or(&Value::Null)));
        let update_time = epoch_to_iso(d_obj.get("update_time").unwrap_or(&Value::Null))
            .or_else(|| epoch_to_iso(listing_row.get("update_time").unwrap_or(&Value::Null)));

        let title = d_obj
            .get("title")
            .and_then(Value::as_str)
            .or_else(|| listing_row.get("title").and_then(Value::as_str))
            .map(String::from);

        let mut conv_raw = d_obj.clone();
        conv_raw.remove("mapping");

        out.conversations.push(OAConversationRow {
            account_id: account_id.clone(),
            conversation_id: cid.clone(),
            title,
            create_time,
            update_time,
            current_node: d_obj
                .get("current_node")
                .and_then(Value::as_str)
                .map(String::from),
            default_model_slug: d_obj
                .get("default_model_slug")
                .and_then(Value::as_str)
                .map(String::from),
            gizmo_id: d_obj
                .get("gizmo_id")
                .and_then(Value::as_str)
                .map(String::from),
            gizmo_type: d_obj
                .get("gizmo_type")
                .and_then(Value::as_str)
                .map(String::from),
            is_archived: d_obj.get("is_archived").and_then(Value::as_bool),
            is_starred: d_obj.get("is_starred").and_then(Value::as_bool),
            raw_json: Value::Object(conv_raw),
        });

        let Some(mapping) = d_obj.get("mapping").and_then(Value::as_object) else {
            continue;
        };
        for (node_id, node) in mapping {
            let Some(node_obj) = node.as_object() else {
                continue;
            };
            let Some(m) = node_obj.get("message").and_then(Value::as_object) else {
                continue;
            };
            let mid = m
                .get("id")
                .and_then(Value::as_str)
                .map(String::from)
                .unwrap_or_else(|| node_id.clone());
            let content = m.get("content");
            let author = m.get("author").and_then(Value::as_object);
            let meta = m.get("metadata").and_then(Value::as_object);

            let content_type = content
                .and_then(Value::as_object)
                .and_then(|c| c.get("content_type"))
                .and_then(Value::as_str)
                .map(String::from);

            let text = synthesize_text(content);

            let mut msg_raw = m.clone();
            msg_raw.remove("content");

            out.messages.push(OAMessageRow {
                conversation_id: cid.clone(),
                message_id: mid.clone(),
                parent_id: node_obj
                    .get("parent")
                    .and_then(Value::as_str)
                    .map(String::from),
                role: author
                    .and_then(|a| a.get("role"))
                    .and_then(Value::as_str)
                    .map(String::from),
                recipient: m.get("recipient").and_then(Value::as_str).map(String::from),
                channel: m.get("channel").and_then(Value::as_str).map(String::from),
                content_type,
                text,
                status: m.get("status").and_then(Value::as_str).map(String::from),
                end_turn: m.get("end_turn").and_then(Value::as_bool),
                weight: m.get("weight").and_then(Value::as_f64),
                model_slug: meta
                    .and_then(|x| x.get("model_slug"))
                    .and_then(Value::as_str)
                    .map(String::from),
                create_time: epoch_to_iso(m.get("create_time").unwrap_or(&Value::Null)),
                update_time: epoch_to_iso(m.get("update_time").unwrap_or(&Value::Null)),
                raw_json: Value::Object(msg_raw),
            });

            out.content_parts.extend(content_parts(&mid, content));
        }
    }

    Ok(out)
}
