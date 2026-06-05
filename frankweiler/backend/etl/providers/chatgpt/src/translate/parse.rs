//! Port of `src/ingest/providers/openai/parse.py`. Reads the doltlite
//! database written by [`crate::extract`] (or, when no DB is present,
//! the legacy JSON tree under
//! `me.json` + `conversations.json` + `conversations/<id>.json`) and
//! flattens it into typed rows.
//!
//! `raw_json` fields carry the JSON minus whatever has been exploded
//! into sibling row types — e.g. conversations drop `mapping`,
//! messages drop `content` — so the row payload stays bounded.

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{Map, Value};

use crate::extract::db::{block_on_load_all, db_path_for, LoadedConversation, LoadedRaw};
use super::sentinels::clean_text;

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
    /// Surfaced attachments — both `metadata.attachments[]` entries and
    /// `image_asset_pointer` parts in `multimodal_text` content. The
    /// renderer emits one link per attachment.
    pub attachments: Vec<OAAttachmentRef>,
}

#[derive(Debug, Clone)]
pub struct OAAttachmentRef {
    pub file_id: String,
    pub name: Option<String>,
    pub mime_type: Option<String>,
    pub is_image: bool,
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

/// One conversation as it sits between extract and render: the upstream
/// JSON payload (full, untouched — used for fingerprinting and for
/// on-demand shredding into messages/parts) paired with the surfaced
/// `OAConversationRow` metadata.
///
/// Translate is per-conversation: render fingerprints the payload,
/// skips it against the indexer's prior fingerprint, and only shreds
/// the mapping into messages+parts when it has to render. That keeps
/// the steady-state translate near-free for unchanged conversations.
#[derive(Debug, Clone)]
pub struct ChatGPTConversation {
    pub conv: OAConversationRow,
    pub upstream_payload: Value,
}

/// Shredded form of one conversation. Built by [`shred`] only for
/// conversations that have actually changed (or are being rendered for
/// the first time).
#[derive(Debug, Clone)]
pub struct ShreddedConversation {
    pub conv: OAConversationRow,
    pub messages: Vec<OAMessageRow>,
    pub content_parts: Vec<OAContentPartRow>,
}

#[derive(Clone)]
pub struct ParsedChatGPTApi {
    pub accounts: Vec<OAAccountRow>,
    pub conversations: Vec<ChatGPTConversation>,
    /// Streaming handle to blob bytes; rendered keyed by upstream
    /// `file_id`. Render materializes these onto disk in a `blobs/`
    /// directory next to each rendered `.md` so the sibling-relative
    /// link resolves.
    pub blobs: std::sync::Arc<dyn frankweiler_etl::blob_store::BlobStore>,
}

impl Default for ParsedChatGPTApi {
    fn default() -> Self {
        Self {
            accounts: Vec::new(),
            conversations: Vec::new(),
            blobs: frankweiler_etl::blob_store::InMemoryBlobStore::empty_handle(),
        }
    }
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
            clean_text(&out.join("\n"))
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
            clean_text(&out.join("\n\n"))
        }
        Some("reasoning_recap") => clean_text(
            content
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or(""),
        ),
        Some("model_editable_context") => clean_text(
            content
                .get("model_set_context")
                .and_then(Value::as_str)
                .unwrap_or(""),
        ),
        _ => String::new(),
    }
}

fn collect_attachments(m: &Map<String, Value>) -> Vec<OAAttachmentRef> {
    let mut out: Vec<OAAttachmentRef> = Vec::new();
    if let Some(arr) = m
        .get("metadata")
        .and_then(Value::as_object)
        .and_then(|md| md.get("attachments"))
        .and_then(Value::as_array)
    {
        for a in arr {
            let Some(id) = a.get("id").and_then(Value::as_str) else {
                continue;
            };
            let mime = a
                .get("mime_type")
                .or_else(|| a.get("mimeType"))
                .and_then(Value::as_str)
                .map(String::from);
            let is_image = mime.as_deref().is_some_and(|s| s.starts_with("image/"));
            out.push(OAAttachmentRef {
                file_id: id.to_string(),
                name: a.get("name").and_then(Value::as_str).map(String::from),
                mime_type: mime,
                is_image,
            });
        }
    }
    if let Some(parts) = m
        .get("content")
        .and_then(Value::as_object)
        .and_then(|c| c.get("parts"))
        .and_then(Value::as_array)
    {
        for p in parts {
            let Some(obj) = p.as_object() else { continue };
            if obj.get("content_type").and_then(Value::as_str) != Some("image_asset_pointer") {
                continue;
            }
            let Some(ptr) = obj.get("asset_pointer").and_then(Value::as_str) else {
                continue;
            };
            let id = ptr
                .strip_prefix("sediment://")
                .or_else(|| ptr.strip_prefix("file-service://"))
                .unwrap_or(ptr);
            if out.iter().any(|a| a.file_id == id) {
                continue;
            }
            out.push(OAAttachmentRef {
                file_id: id.to_string(),
                name: None,
                mime_type: None,
                is_image: true,
            });
        }
    }
    out
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
                            text: Some(clean_text(s)),
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
                            text: Some(clean_text(&txt)),
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
                        text: Some(clean_text(&bits.join("\n\n"))),
                        raw_json: t.clone(),
                    });
                }
            }
        }
        Some("reasoning_recap") => {
            let text = content
                .get("content")
                .and_then(Value::as_str)
                .unwrap_or("");
            rows.push(OAContentPartRow {
                message_id: message_id.into(),
                part_index: 0,
                kind: "reasoning_recap".into(),
                language: None,
                text: Some(clean_text(text)),
                raw_json: Value::Object(content.clone()),
            });
        }
        Some("model_editable_context") => {
            rows.push(OAContentPartRow {
                message_id: message_id.into(),
                part_index: 0,
                kind: "model_editable_context".into(),
                language: None,
                text: Some(clean_text(
                    content
                        .get("model_set_context")
                        .and_then(Value::as_str)
                        .unwrap_or(""),
                )),
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

/// Read raw payloads out of the doltlite DB. If `path` doesn't resolve
/// to an existing `.doltlite_db`, falls back to walking a legacy JSON
/// tree under `path/` — that's the shape the in-tree TNG render-test
/// fixture still uses.
///
/// The DB path requires a tokio runtime (uses
/// [`crate::extract::db::block_on_load_all`]); the bin entry point
/// wraps `main` with `#[tokio::main]` for this reason.
pub fn parse_api_dir(path: &Path) -> Result<ParsedChatGPTApi> {
    let db_path = db_path_for(path);
    if db_path.exists() {
        let raw = block_on_load_all(&db_path)
            .with_context(|| format!("load chatgpt db {}", db_path.display()))?;
        return Ok(parse_loaded(raw));
    }
    if path.is_dir() {
        return parse_api_json_dir(path);
    }
    // Neither a DB nor a JSON tree — caller's path is wrong; surface a
    // clear error instead of silently returning empty rows.
    anyhow::bail!(
        "chatgpt source not found at {} (no .doltlite_db, no JSON tree)",
        path.display()
    )
}

/// Build a [`ParsedChatGPTApi`] from a snapshot already loaded out of
/// the doltlite DB.
pub fn parse_loaded(raw: LoadedRaw) -> ParsedChatGPTApi {
    let mut out = ParsedChatGPTApi {
        blobs: raw.blobs,
        ..Default::default()
    };
    let account_id = if let Some(me) = raw.me.as_ref() {
        let id_opt = me.get("id").and_then(Value::as_str).map(String::from);
        if let Some(id) = id_opt.clone() {
            out.accounts.push(OAAccountRow {
                account_id: id,
                email: me.get("email").and_then(Value::as_str).map(String::from),
                name: me.get("name").and_then(Value::as_str).map(String::from),
                raw_json: me.clone(),
            });
        }
        id_opt
    } else {
        None
    };
    for LoadedConversation {
        id: _,
        payload,
        last_listing_update_time,
        ..
    } in raw.conversations
    {
        let Some(conv) = build_conv_row(&payload, last_listing_update_time.as_ref(), &account_id)
        else {
            continue;
        };
        out.conversations.push(ChatGPTConversation {
            conv,
            upstream_payload: payload,
        });
    }
    out
}

/// Legacy fallback: walk a `me.json` / `conversations.json` /
/// `conversations/<id>.json` tree. Kept for the in-crate TNG fixture
/// used by the render golden test, which we'd rather not regenerate as
/// a binary doltlite_db every time the source data changes.
pub fn parse_api_json_dir(api_dir: &Path) -> Result<ParsedChatGPTApi> {
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
    let mut listing_by_id: HashMap<String, Value> = HashMap::new();
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
        let listing_row = listing_by_id.get(&cid).cloned();
        let Some(mut conv) = build_conv_row(&d, listing_row.as_ref(), &account_id) else {
            continue;
        };
        // The legacy reader keys off the file name when the JSON
        // didn't carry an `id`/`conversation_id` — preserve that shape.
        if conv.conversation_id.is_empty() {
            conv.conversation_id = cid;
        }
        out.conversations.push(ChatGPTConversation {
            conv,
            upstream_payload: d,
        });
    }

    Ok(out)
}

/// Build the `OAConversationRow` metadata for one upstream payload.
/// Returns `None` if `payload` isn't a JSON object. The conversation's
/// `mapping` (containing every message + content part) is *not* walked
/// here — that work is deferred to [`shred`] so unchanged conversations
/// never pay it.
pub fn build_conv_row(
    payload: &Value,
    listing_row: Option<&Value>,
    account_id: &Option<String>,
) -> Option<OAConversationRow> {
    let d_obj = payload.as_object()?;
    let cid = d_obj
        .get("conversation_id")
        .and_then(Value::as_str)
        .or_else(|| d_obj.get("id").and_then(Value::as_str))
        .map(String::from)
        .unwrap_or_default();

    let empty = Value::Object(Map::new());
    let listing = listing_row.unwrap_or(&empty);

    let create_time = epoch_to_iso(d_obj.get("create_time").unwrap_or(&Value::Null))
        .or_else(|| epoch_to_iso(listing.get("create_time").unwrap_or(&Value::Null)));
    let update_time = epoch_to_iso(d_obj.get("update_time").unwrap_or(&Value::Null))
        .or_else(|| epoch_to_iso(listing.get("update_time").unwrap_or(&Value::Null)));

    let title = d_obj
        .get("title")
        .and_then(Value::as_str)
        .or_else(|| listing.get("title").and_then(Value::as_str))
        .map(String::from);

    let mut conv_raw = d_obj.clone();
    conv_raw.remove("mapping");

    Some(OAConversationRow {
        account_id: account_id.clone(),
        conversation_id: cid,
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
    })
}

/// Walk a conversation's `mapping` and emit its messages and content
/// parts. Only called for conversations the renderer is actually going
/// to re-render — for unchanged conversations the fingerprint check
/// short-circuits and we never visit the mapping at all.
pub fn shred(c: &ChatGPTConversation) -> ShreddedConversation {
    let mut messages = Vec::new();
    let mut content_parts_out = Vec::new();
    let cid = c.conv.conversation_id.as_str();

    if let Some(mapping) = c
        .upstream_payload
        .as_object()
        .and_then(|o| o.get("mapping"))
        .and_then(Value::as_object)
    {
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

            let attachments = collect_attachments(m);

            messages.push(OAMessageRow {
                conversation_id: cid.to_string(),
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
                attachments,
            });

            content_parts_out.extend(content_parts(&mid, content));
        }
    }

    ShreddedConversation {
        conv: c.conv.clone(),
        messages,
        content_parts: content_parts_out,
    }
}
