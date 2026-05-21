//! Port of `_render_one_anthropic` + `_render_anthropic_block` +
//! shared helpers from `src/ingest/render.py`. Output must match the
//! Python byte-for-byte (the project's UI loads the rendered .md
//! verbatim, so any drift visibly mis-renders messages).

use std::collections::HashMap;

use chrono::{DateTime, FixedOffset};
use once_cell::sync::Lazy;
use serde_json::{json, Value};

use frankweiler_etl::media::{media_relpath, relative_link, safe_filename};
use frankweiler_etl::sidecar::{Sidecar, SidecarHeader};

use super::grid_rows::{fingerprint_for_conversation, rows_for_conversation, RENDER_VERSION};
use super::parse::{AttachmentRow, ContentBlockRow, ConversationRow, MessageRow, ParsedExport};

/// YAML scalar emitter that matches the Python `_yaml_scalar` exactly.
/// Strings containing structural chars (`:#\n"'`) or with surrounding
/// whitespace get JSON-escaped (Python uses `json.dumps(..., ensure_ascii=False)`).
pub(crate) fn yaml_scalar(v: Option<&str>) -> String {
    let Some(s) = v else {
        return "null".into();
    };
    let needs_quote = s
        .chars()
        .any(|c| matches!(c, ':' | '#' | '\n' | '"' | '\''))
        || s != s.trim();
    if needs_quote {
        // serde_json's `to_string` with a String input produces the same
        // escapes as Python's json.dumps with ensure_ascii=False (Rust
        // serde_json defaults to UTF-8 passthrough for non-control chars).
        serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
    } else {
        s.into()
    }
}

/// Same for non-string YAML scalars rendered from Python `str(v)`.
fn yaml_scalar_raw(s: &str) -> String {
    yaml_scalar(Some(s))
}

pub(crate) fn bump_iso(ts: &str) -> String {
    let parse_input = if let Some(prefix) = ts.strip_suffix('Z') {
        format!("{prefix}+00:00")
    } else {
        ts.to_string()
    };
    let Ok(dt) = DateTime::<FixedOffset>::parse_from_rfc3339(&parse_input) else {
        return ts.to_string();
    };
    let bumped = dt + chrono::Duration::microseconds(1);
    // Match Python isoformat: "+00:00" suffix; chrono RFC3339 emits the same.
    let mut out = bumped.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, false);
    if ts.ends_with('Z') && out.ends_with("+00:00") {
        out.truncate(out.len() - 6);
        out.push('Z');
    }
    out
}

pub(crate) fn msg_div_open(msg_uuid: &str, msg_index: usize, provider: &str) -> String {
    format!(
        "<div id=\"m-{msg_uuid}\" data-msg-index=\"{msg_index}\" class=\"msg msg--{provider}\">"
    )
}

pub(crate) const MSG_DIV_CLOSE: &str = "</div>";

/// JSON dumped the way Python emits with `indent=2, ensure_ascii=False, sort_keys=True`.
/// serde_json::to_string_pretty with a BTreeMap-backed Value matches when keys
/// are sorted; we sort all object keys recursively before serializing.
fn json_pretty_sorted(v: &Value) -> String {
    let canonical = canonicalize(v);
    // serde_json::to_string_pretty uses 2-space indent by default.
    serde_json::to_string_pretty(&canonical).unwrap_or_default()
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

fn render_anthropic_block(
    msg_uuid: &str,
    block_index: usize,
    btype: Option<&str>,
    btext: Option<&str>,
    braw: &Value,
) -> Vec<String> {
    let raw_obj: &serde_json::Map<String, Value> = match braw {
        Value::Object(m) => m,
        Value::String(s) if !s.is_empty() => {
            // Re-parse as a JSON object if possible.
            if let Ok(Value::Object(m)) = serde_json::from_str::<Value>(s) {
                return render_anthropic_block(
                    msg_uuid,
                    block_index,
                    btype,
                    btext,
                    &Value::Object(m),
                );
            }
            &EMPTY_MAP
        }
        _ => &EMPTY_MAP,
    };

    let mut anchors = vec![format!("<a id=\"b-{msg_uuid}-{block_index}\"></a>")];
    if btype == Some("tool_use") {
        if let Some(id) = raw_obj.get("id").and_then(Value::as_str) {
            anchors.push(format!("<a id=\"tu-{id}\"></a>"));
        }
    } else if btype == Some("tool_result") {
        if let Some(id) = raw_obj.get("tool_use_id").and_then(Value::as_str) {
            anchors.push(format!("<a id=\"tr-{id}\"></a>"));
        }
    }
    let head = anchors.join("");

    match btype {
        Some("text") => {
            if let Some(text) = btext {
                return vec![format!("{head}{}", text.trim_end()), String::new()];
            }
        }
        Some("thinking") => {
            let thought = raw_obj
                .get("thinking")
                .and_then(Value::as_str)
                .or(btext)
                .unwrap_or("");
            if thought.is_empty() {
                return vec![format!("{head}<!-- thinking (no text) -->"), String::new()];
            }
            let quoted = format!("> {}", thought.trim_end().replace('\n', "\n> "));
            return vec![
                format!("{head}<details><summary>Thinking</summary>"),
                String::new(),
                quoted,
                String::new(),
                "</details>".into(),
                String::new(),
            ];
        }
        Some("tool_use") => {
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
                format!("{head}<details><summary>{summary}</summary>"),
                String::new(),
            ];
            if let Some(tool_input) = raw_obj.get("input") {
                if !tool_input.is_null() {
                    // Falsy-ish check mirroring Python `if tool_input:` —
                    // skip if empty object/array/string.
                    let empty = match tool_input {
                        Value::Object(m) => m.is_empty(),
                        Value::Array(a) => a.is_empty(),
                        Value::String(s) => s.is_empty(),
                        Value::Bool(false) | Value::Null => true,
                        Value::Number(n) => n.as_f64() == Some(0.0),
                        _ => false,
                    };
                    if !empty {
                        out.push("```json".into());
                        out.push(json_pretty_sorted(tool_input));
                        out.push("```".into());
                    }
                }
            }
            out.push("</details>".into());
            out.push(String::new());
            return out;
        }
        Some("tool_result") => {
            let name = raw_obj
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("tool");
            let is_err = raw_obj
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            let content = raw_obj.get("content");
            let summary = if is_err {
                format!("Tool result: {name} (error)")
            } else {
                format!("Tool result: {name}")
            };
            let mut out = vec![
                format!("{head}<details><summary>{summary}</summary>"),
                String::new(),
            ];
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
                                let t = m.get("text").and_then(Value::as_str).unwrap();
                                out.push(t.trim_end().into());
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
            out.push("</details>".into());
            out.push(String::new());
            return out;
        }
        _ => {}
    }

    if let Some(text) = btext {
        if !text.is_empty() {
            let fence = format!("```{}", btype.unwrap_or("")).trim_end().to_string();
            return vec![
                head,
                fence,
                text.trim_end().into(),
                "```".into(),
                String::new(),
            ];
        }
    }
    vec![
        format!("{head}<!-- {} (no text) -->", btype.unwrap_or("block")),
        String::new(),
    ]
}

static EMPTY_MAP: Lazy<serde_json::Map<String, Value>> = Lazy::new(serde_json::Map::new);

#[allow(dead_code)]
fn _ensure_json_used(_: Value) {
    let _ = json!({});
}

pub struct Rendered {
    pub conversation_uuid: String,
    pub account_uuid: String,
    pub body: String,
}

impl Rendered {
    pub fn relative_path(&self) -> std::path::PathBuf {
        std::path::PathBuf::from("rendered_md/anthropic")
            .join(&self.account_uuid)
            .join("llm_chats")
            .join(format!("{}.md", self.conversation_uuid))
    }
}

pub fn render_all(
    parsed: &ParsedExport,
    root: &std::path::Path,
    source_name: &str,
) -> std::io::Result<Vec<std::path::PathBuf>> {
    let mut written = Vec::new();
    for conv in &parsed.conversations {
        let Some(r) = render_one(parsed, &conv.conversation_uuid, source_name) else {
            continue;
        };
        let rel = r.relative_path();
        let abs = root.join(&rel);
        if let Some(dir) = abs.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&abs, &r.body)?;

        let rows = rows_for_conversation(parsed, &conv.conversation_uuid);
        let sidecar = Sidecar {
            header: SidecarHeader {
                document_uuid: conv.conversation_uuid.clone(),
                source_fingerprint: fingerprint_for_conversation(parsed, &conv.conversation_uuid),
                render_version: RENDER_VERSION,
            },
            rows,
        };
        let sidecar_abs = abs.with_extension("grid_rows.json");
        let sidecar_json = serde_json::to_string_pretty(&sidecar).map_err(std::io::Error::other)?;
        std::fs::write(&sidecar_abs, sidecar_json)?;

        written.push(rel);
    }
    Ok(written)
}

pub fn render_one(parsed: &ParsedExport, conv_uuid: &str, source_name: &str) -> Option<Rendered> {
    let conv: &ConversationRow = parsed
        .conversations
        .iter()
        .find(|c| c.conversation_uuid == conv_uuid)?;

    let mut blocks_by_msg: HashMap<&str, Vec<&ContentBlockRow>> = HashMap::new();
    for b in &parsed.content_blocks {
        blocks_by_msg.entry(&b.message_uuid).or_default().push(b);
    }
    let mut atts_by_msg: HashMap<&str, Vec<&AttachmentRow>> = HashMap::new();
    for a in &parsed.attachments {
        atts_by_msg.entry(&a.message_uuid).or_default().push(a);
    }
    let mut msgs: Vec<&MessageRow> = parsed
        .messages
        .iter()
        .filter(|m| m.conversation_uuid == conv_uuid)
        .collect();
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

    let mut parts: Vec<String> = Vec::new();
    parts.push("---".into());
    parts.push("provider: anthropic".into());
    parts.push(format!("uuid: {}", yaml_scalar(Some(conv_uuid))));
    parts.push(format!("name: {}", yaml_scalar(conv.name.as_deref())));
    parts.push(format!(
        "account_uuid: {}",
        yaml_scalar(Some(&conv.account_uuid))
    ));
    parts.push(format!(
        "project_uuid: {}",
        yaml_scalar(conv.project_uuid.as_deref())
    ));
    parts.push(format!(
        "created_at: {}",
        yaml_scalar(conv.created_at.as_deref())
    ));
    parts.push(format!(
        "updated_at: {}",
        yaml_scalar(conv.updated_at.as_deref())
    ));
    if let Some(summary) = conv.summary.as_deref() {
        if !summary.is_empty() {
            parts.push(format!("summary: {}", yaml_scalar_raw(summary)));
        }
    }
    parts.push("---".into());
    parts.push(String::new());
    parts.push(format!(
        "# {}",
        conv.name.as_deref().unwrap_or("(untitled)")
    ));
    parts.push(String::new());

    let mut last_ts: Option<String> = conv.created_at.clone();
    for (msg_index, m) in msgs.iter().enumerate() {
        let mut msg_created = m.created_at.clone();
        if msg_created.is_none() {
            if let Some(prev) = &last_ts {
                msg_created = Some(bump_iso(prev));
            }
        }
        if let Some(ts) = &msg_created {
            last_ts = Some(ts.clone());
        }
        let heading = capitalize(m.sender.as_deref().unwrap_or("unknown"));
        parts.push(msg_div_open(&m.message_uuid, msg_index, "anthropic"));
        parts.push(String::new());
        parts.push(format!("## {heading}"));
        if let Some(ts) = &msg_created {
            parts.push(String::new());
            parts.push(format!("*{ts}*"));
        }
        parts.push(String::new());

        let mut blocks = blocks_by_msg
            .get(m.message_uuid.as_str())
            .cloned()
            .unwrap_or_default();
        blocks.sort_by_key(|b| b.block_index);
        for b in blocks {
            parts.extend(render_anthropic_block(
                &m.message_uuid,
                b.block_index,
                b.r#type.as_deref(),
                b.text.as_deref(),
                &b.raw_json,
            ));
        }

        let mut atts = atts_by_msg
            .get(m.message_uuid.as_str())
            .cloned()
            .unwrap_or_default();
        atts.sort_by_key(|a| a.attachment_index);
        if !atts.is_empty() {
            let md_rel = std::path::PathBuf::from("rendered_md/anthropic")
                .join(&conv.account_uuid)
                .join("llm_chats")
                .join(format!("{conv_uuid}.md"));
            parts.push("**Attachments:**".into());
            parts.push(String::new());
            for at in atts {
                parts.push(format!("- {}", attachment_md(source_name, &md_rel, at)));
            }
            parts.push(String::new());
        }
        parts.push(MSG_DIV_CLOSE.into());
        parts.push(String::new());
    }

    let mut body = parts.join("\n");
    while body.ends_with('\n') || body.ends_with('\r') {
        body.pop();
    }
    body.push('\n');

    Some(Rendered {
        conversation_uuid: conv_uuid.into(),
        account_uuid: conv.account_uuid.clone(),
        body,
    })
}

/// Render one attachment as a markdown link into
/// `raw/<source_name>/media/<id>/<name>` (the canonical staged path).
/// Images get `![alt](link)`; everything else becomes `[\[file\] alt](link)`.
/// Falls back to a plain label when the upstream record lacks an id.
fn attachment_md(source_name: &str, md_rel: &std::path::Path, at: &AttachmentRow) -> String {
    let raw_obj = at.raw_json.as_object();
    let label = raw_obj
        .and_then(|o| {
            o.get("file_name")
                .or_else(|| o.get("name"))
                .or_else(|| o.get("file_kind"))
        })
        .and_then(Value::as_str)
        .unwrap_or("(unnamed)")
        .to_string();
    let id = raw_obj
        .and_then(|o| {
            o.get("file_uuid")
                .or_else(|| o.get("id"))
                .or_else(|| o.get("uuid"))
        })
        .and_then(Value::as_str)
        .map(String::from);
    let is_image = raw_obj
        .and_then(|o| o.get("file_kind").or_else(|| o.get("file_type")))
        .and_then(Value::as_str)
        .map(|s| s.eq_ignore_ascii_case("image") || s.starts_with("image/"))
        .unwrap_or(false);
    let Some(id) = id else {
        return format!("[{}] {}", at.kind, label);
    };
    let name = safe_filename(Some(&label), &id);
    let target = media_relpath(source_name, &id, &name);
    let link = relative_link(md_rel, &target);
    let alt = label.replace(']', "");
    if is_image {
        format!("![{alt}]({link})")
    } else {
        format!("[\\[file\\] {alt}]({link})")
    }
}

fn capitalize(s: &str) -> String {
    // Python `str.capitalize()`: first char upper, rest lower.
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
