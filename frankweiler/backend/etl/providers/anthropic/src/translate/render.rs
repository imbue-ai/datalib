//! Port of `_render_one_anthropic` + `_render_anthropic_block` +
//! shared helpers from `src/ingest/render.py`. Output must match the
//! Python byte-for-byte (the project's UI loads the rendered .md
//! verbatim, so any drift visibly mis-renders messages).

use std::collections::HashMap;

use chrono::{DateTime, FixedOffset};
use once_cell::sync::Lazy;
use serde_json::{json, Value};

use frankweiler_etl::blobs::safe_filename;
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl::sidecar::{Sidecar, SidecarHeader};

use frankweiler_etl::blob_store::BlobStore;

use super::grid_rows::{fingerprint_for_conversation, rows_for_conversation, RENDER_VERSION};
use super::parse::{
    shred, AttachmentRow, ContentBlockRow, MessageRow, ParsedExport, ShreddedConversation,
};

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

/// Open tag for a per-message section. The UI keys selection /
/// scroll-to / copy-uuid off `data-section-uuid` — that's the single
/// source of truth shared with the grid row's `uuid` column, so it
/// can't drift the way the old `data-msg-index` did.
pub(crate) fn msg_div_open(msg_uuid: &str, provider: &str) -> String {
    format!(
        "<div id=\"m-{msg_uuid}\" data-section-uuid=\"{msg_uuid}\" class=\"msg msg--{provider}\">"
    )
}

/// Open tag for a per-block section nested inside a message
/// (`tool_use`, `tool_result`, `thinking`). The section's id and
/// `data-section-uuid` always match the grid row uuid the translator
/// emits — see `grid_rows::rows_for_conversation` and the
/// `tu-`/`tr-`/`th-` prefix convention there.
pub(crate) fn block_div_open(section_uuid: &str, block_kind: &str) -> String {
    format!(
        "<div id=\"{section_uuid}\" data-section-uuid=\"{section_uuid}\" class=\"msg msg--block msg--{block_kind}\">"
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

/// The grid-row `uuid` (and matching section-div `id` /
/// `data-section-uuid`) for a content block. Returns `None` for blocks
/// that don't get their own row/section (e.g. plain `text`, which is
/// inline in the parent message).
///
/// Prefix convention: `tu-` for `tool_use` (Anthropic's
/// `tool_use_id` is the natural id), `tr-` for the matching
/// `tool_result`, `th-` for `thinking` (no upstream id —
/// `{msg_uuid}-{block_index}` is the unavoidable synthesis).
pub(crate) fn section_uuid_for_block(
    msg_uuid: &str,
    block_index: usize,
    btype: Option<&str>,
    raw_obj: &serde_json::Map<String, Value>,
) -> Option<String> {
    match btype {
        Some("tool_use") => raw_obj
            .get("id")
            .and_then(Value::as_str)
            .map(|id| format!("tu-{id}")),
        Some("tool_result") => raw_obj
            .get("tool_use_id")
            .and_then(Value::as_str)
            .map(|id| format!("tr-{id}")),
        Some("thinking") => Some(format!("th-{msg_uuid}-{block_index}")),
        _ => None,
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

    // Blocks that get their own section get wrapped in a div whose id
    // matches the grid row's uuid (see grid_rows::rows_for_conversation
    // — that's the contract the UI relies on to highlight the right
    // block when its row is clicked). Plain `text` blocks stay inline.
    let section = section_uuid_for_block(msg_uuid, block_index, btype, raw_obj);
    let (section_open, section_close) = match (&section, btype) {
        (Some(sid), Some("tool_use")) => {
            (Some(block_div_open(sid, "tool-use")), Some(MSG_DIV_CLOSE))
        }
        (Some(sid), Some("tool_result")) => (
            Some(block_div_open(sid, "tool-result")),
            Some(MSG_DIV_CLOSE),
        ),
        (Some(sid), Some("thinking")) => {
            (Some(block_div_open(sid, "thinking")), Some(MSG_DIV_CLOSE))
        }
        _ => (None, None),
    };

    let body: Vec<String> = match btype {
        Some("text") => {
            if let Some(text) = btext {
                vec![text.trim_end().into(), String::new()]
            } else {
                vec![
                    format!("<!-- {} (no text) -->", btype.unwrap_or("block")),
                    String::new(),
                ]
            }
        }
        Some("thinking") => {
            let thought = raw_obj
                .get("thinking")
                .and_then(Value::as_str)
                .or(btext)
                .unwrap_or("");
            if thought.is_empty() {
                vec!["<!-- thinking (no text) -->".into(), String::new()]
            } else {
                let quoted = format!("> {}", thought.trim_end().replace('\n', "\n> "));
                vec![
                    "<details><summary>Thinking</summary>".into(),
                    String::new(),
                    quoted,
                    String::new(),
                    "</details>".into(),
                    String::new(),
                ]
            }
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
                format!("<details><summary>{summary}</summary>"),
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
            out
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
                format!("<details><summary>{summary}</summary>"),
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
            out
        }
        _ => {
            if let Some(text) = btext {
                if !text.is_empty() {
                    let fence = format!("```{}", btype.unwrap_or("")).trim_end().to_string();
                    vec![fence, text.trim_end().into(), "```".into(), String::new()]
                } else {
                    vec![
                        format!("<!-- {} (no text) -->", btype.unwrap_or("block")),
                        String::new(),
                    ]
                }
            } else {
                vec![
                    format!("<!-- {} (no text) -->", btype.unwrap_or("block")),
                    String::new(),
                ]
            }
        }
    };

    match (section_open, section_close) {
        (Some(open), Some(close)) => {
            let mut out = Vec::with_capacity(body.len() + 3);
            out.push(open);
            out.push(String::new());
            out.extend(body);
            out.push(close.into());
            out.push(String::new());
            out
        }
        _ => body,
    }
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
    /// Page-dir layout: `<conv_uuid>/index.md`. Blobs live in a
    /// sibling `blobs/` subdir under the same dir so a conversation
    /// is sharable in isolation. Mirrors Notion's `<page_dir>/index.md`
    /// shape.
    pub fn relative_path(&self) -> std::path::PathBuf {
        std::path::PathBuf::from("rendered_md/anthropic")
            .join(&self.account_uuid)
            .join("llm_chats")
            .join(&self.conversation_uuid)
            .join("index.md")
    }
}

pub fn render_all(
    parsed: &ParsedExport,
    root: &std::path::Path,
    source_name: &str,
    progress: &Progress,
    prior_fingerprints: &std::collections::HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> anyhow::Result<()>,
) -> anyhow::Result<Vec<std::path::PathBuf>> {
    progress.set_length(Some(parsed.conversations.len() as u64));
    let mut written = Vec::new();
    for c in &parsed.conversations {
        let fingerprint = fingerprint_for_conversation(&c.upstream_payload);
        let conv_uuid = c.conv.conversation_uuid.clone();
        let rel = conv_relative_path(&c.conv.account_uuid, &conv_uuid);
        let abs = root.join(&rel);

        // Skip when the indexer has the same fingerprint AND the md
        // file is still on disk. No shredding happens here — the
        // upstream payload alone is enough to decide.
        if prior_fingerprints.get(&conv_uuid).map(String::as_str) == Some(fingerprint.as_str())
            && abs.exists()
        {
            written.push(rel);
            progress.inc(1);
            continue;
        }

        // Changed (or first-time): walk chat_messages into msgs/blocks/atts.
        let shredded = shred(c);
        let Some(r) = render_one(&shredded, source_name) else {
            progress.inc(1);
            continue;
        };

        let page_dir = abs
            .parent()
            .expect("relative_path always has a parent (the page-dir)");
        std::fs::create_dir_all(page_dir)?;

        // Order: blobs → md → sidecar → callback. Callback firing last
        // is the commit point.
        materialize_conv_blobs(&shredded, parsed.blobs.as_ref(), page_dir)?;

        std::fs::write(&abs, &r.body)?;

        let rows = rows_for_conversation(&shredded);
        let sidecar = Sidecar {
            header: SidecarHeader {
                markdown_uuid: conv_uuid.clone(),
                source_fingerprint: fingerprint.clone(),
                render_version: RENDER_VERSION,
            },
            rows: rows.clone(),
        };
        let sidecar_abs = abs.with_extension("grid_rows.json");
        let sidecar_json = serde_json::to_string_pretty(&sidecar).map_err(std::io::Error::other)?;
        std::fs::write(&sidecar_abs, sidecar_json)?;

        on_doc_complete(RenderedMarkdown {
            markdown_uuid: conv_uuid.clone(),
            source_name: source_name.to_string(),
            source_fingerprint: fingerprint,
            upstream_cursor: None,
            md_path: abs.clone(),
            render_version: RENDER_VERSION,
            rows,
        })?;

        written.push(rel);
        progress.inc(1);
    }
    Ok(written)
}

/// Mirror of `Rendered::relative_path` for use *before* we've rendered
/// (so we can fingerprint-skip without paying for a render first).
fn conv_relative_path(account_uuid: &str, conv_uuid: &str) -> std::path::PathBuf {
    std::path::PathBuf::from("rendered_md/anthropic")
        .join(account_uuid)
        .join("llm_chats")
        .join(conv_uuid)
        .join("index.md")
}

pub fn render_one(shredded: &ShreddedConversation, _source_name: &str) -> Option<Rendered> {
    let conv = &shredded.conv;
    let conv_uuid = conv.conversation_uuid.as_str();

    let mut blocks_by_msg: HashMap<&str, Vec<&ContentBlockRow>> = HashMap::new();
    for b in &shredded.content_blocks {
        blocks_by_msg.entry(&b.message_uuid).or_default().push(b);
    }
    let mut atts_by_msg: HashMap<&str, Vec<&AttachmentRow>> = HashMap::new();
    for a in &shredded.attachments {
        atts_by_msg.entry(&a.message_uuid).or_default().push(a);
    }
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
    for m in msgs.iter() {
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
        parts.push(msg_div_open(&m.message_uuid, "anthropic"));
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
            parts.push("**Attachments:**".into());
            parts.push(String::new());
            for at in atts {
                parts.push(format!("- {}", attachment_md(at)));
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

/// Write every blob this conversation references into
/// `<page_dir>/blobs/<filename>`. The markdown emitted by
/// `attachment_md` uses the same `safe_filename(name, id)` rule so the
/// link target matches what lands on disk.
fn materialize_conv_blobs(
    shredded: &ShreddedConversation,
    blobs: &dyn BlobStore,
    page_dir: &std::path::Path,
) -> std::io::Result<()> {
    // file_uuid → first name we saw for it, walking only this
    // conversation's attachments. Each conversation owns its blobs in
    // a sibling `blobs/` dir, so there's no need to cross-reference
    // other conversations.
    let mut name_by_id: HashMap<String, Option<String>> = HashMap::new();
    for at in &shredded.attachments {
        let Some(obj) = at.raw_json.as_object() else {
            continue;
        };
        let Some(id) = obj
            .get("file_uuid")
            .or_else(|| obj.get("id"))
            .or_else(|| obj.get("uuid"))
            .and_then(Value::as_str)
        else {
            continue;
        };
        let name = obj
            .get("file_name")
            .or_else(|| obj.get("name"))
            .and_then(Value::as_str)
            .map(String::from);
        name_by_id.entry(id.to_string()).or_insert(name);
    }
    if name_by_id.is_empty() {
        return Ok(());
    }
    let blobs_dir = page_dir.join("blobs");
    for (file_uuid, name) in &name_by_id {
        let blob = match blobs.read_by_id(file_uuid) {
            Ok(Some(b)) => b,
            Ok(None) => continue,
            Err(e) => return Err(std::io::Error::other(e)),
        };
        let safe = safe_filename(name.as_deref(), file_uuid);
        std::fs::create_dir_all(&blobs_dir)?;
        std::fs::write(blobs_dir.join(&safe), &blob.bytes)?;
    }
    Ok(())
}

/// Render one attachment as a markdown link into
/// `blobs/<filename>` (relative to the conversation's `index.md`).
/// Images get `![alt](link)`; everything else becomes
/// `[\[file\] alt](link)`. Falls back to a plain label when the
/// upstream record lacks an id.
fn attachment_md(at: &AttachmentRow) -> String {
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
    let safe = safe_filename(Some(&label), &id);
    let link = format!("blobs/{safe}");
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
