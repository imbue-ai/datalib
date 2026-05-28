//! Port of `_render_one_openai` from `src/ingest/render.py`, plus the
//! shared `_slugify` / `_yaml_scalar` / `_bump_iso` / `_msg_div_open`
//! helpers (kept private here; if the Anthropic crate's copies and
//! these drift, we'll promote them into `frankweiler-etl`).

use std::collections::HashMap;

use chrono::{DateTime, FixedOffset};

use frankweiler_etl::blobs::safe_filename;
use frankweiler_etl::progress::Progress;
use frankweiler_etl::sidecar::{Sidecar, SidecarHeader};

use super::grid_rows::{fingerprint_for_conversation, rows_for_conversation, RENDER_VERSION};
use super::parse::{
    OAAttachmentRef, OAContentPartRow, OAConversationRow, OAMessageRow, ParsedChatGPTApi,
};

fn yaml_scalar(v: Option<&str>) -> String {
    let Some(s) = v else {
        return "null".into();
    };
    let needs_quote = s
        .chars()
        .any(|c| matches!(c, ':' | '#' | '\n' | '"' | '\''))
        || s != s.trim();
    if needs_quote {
        serde_json::to_string(s).unwrap_or_else(|_| format!("\"{s}\""))
    } else {
        s.into()
    }
}

fn bump_iso(ts: &str) -> String {
    let parse_input = if let Some(prefix) = ts.strip_suffix('Z') {
        format!("{prefix}+00:00")
    } else {
        ts.to_string()
    };
    let Ok(dt) = DateTime::<FixedOffset>::parse_from_rfc3339(&parse_input) else {
        return ts.to_string();
    };
    let bumped = dt + chrono::Duration::microseconds(1);
    let mut out = bumped.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, false);
    if ts.ends_with('Z') && out.ends_with("+00:00") {
        out.truncate(out.len() - 6);
        out.push('Z');
    }
    out
}

fn msg_div_open(msg_uuid: &str, provider: &str) -> String {
    format!(
        "<div id=\"m-{msg_uuid}\" data-section-uuid=\"{msg_uuid}\" class=\"msg msg--{provider}\">"
    )
}

const MSG_DIV_CLOSE: &str = "</div>";

fn capitalize(s: &str) -> String {
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

pub struct Rendered {
    pub conversation_id: String,
    pub account_id: String,
    pub body: String,
}

impl Rendered {
    /// Page-dir layout: each conversation is `<conv_id>/index.md` so
    /// its blobs can live in a sibling `blobs/` subdir under the same
    /// dir. Matches Notion's `<page_dir>/index.md` shape so the
    /// rendered tree is internally consistent and a single
    /// `<conv_id>/` directory is sharable in isolation.
    pub fn relative_path(&self) -> std::path::PathBuf {
        std::path::PathBuf::from("rendered_md/openai")
            .join(&self.account_id)
            .join("llm_chats")
            .join(&self.conversation_id)
            .join("index.md")
    }
}

/// Render every conversation into `<root>/rendered_md/...`. Returns the
/// list of paths written. Matches `render_openai` semantics minus the
/// orphan cleanup (the Rust translator's idempotency story rides on
/// `.grid_rows.json` sidecars added in Stage 4).
pub fn render_all(
    parsed: &ParsedChatGPTApi,
    root: &std::path::Path,
    source_name: &str,
    progress: &Progress,
) -> std::io::Result<Vec<std::path::PathBuf>> {
    // Build (file_id → file_name) once so per-conversation blob writes
    // pick the same filename `attachment_md` puts in the markdown link.
    let name_by_id = name_by_id(parsed);

    progress.set_length(Some(parsed.conversations.len() as u64));
    let mut written = Vec::new();
    for conv in &parsed.conversations {
        let Some(r) = render_one(parsed, &conv.conversation_id, source_name) else {
            progress.inc(1);
            continue;
        };
        let rel = r.relative_path();
        let abs = root.join(&rel);
        let page_dir = abs
            .parent()
            .expect("relative_path always has a parent (the page-dir)");
        std::fs::create_dir_all(page_dir)?;

        // Write blob bytes into <page_dir>/blobs/<filename>. The
        // markdown links emitted by `attachment_md` are relative
        // `blobs/<filename>` paths and resolve against this dir.
        materialize_conv_blobs(parsed, conv, &name_by_id, page_dir)?;

        std::fs::write(&abs, &r.body)?;

        let rows = rows_for_conversation(parsed, &conv.conversation_id);
        let sidecar = Sidecar {
            header: SidecarHeader {
                document_uuid: conv.conversation_id.clone(),
                source_fingerprint: fingerprint_for_conversation(parsed, &conv.conversation_id),
                render_version: RENDER_VERSION,
            },
            rows,
        };
        // `abs` is `<page_dir>/index.md`, so with_extension yields
        // `<page_dir>/index.grid_rows.json`.
        let sidecar_abs = abs.with_extension("grid_rows.json");
        let sidecar_json = serde_json::to_string_pretty(&sidecar).map_err(std::io::Error::other)?;
        std::fs::write(&sidecar_abs, sidecar_json)?;

        written.push(rel);
        progress.inc(1);
    }
    Ok(written)
}

pub fn render_one(
    parsed: &ParsedChatGPTApi,
    conversation_id: &str,
    _source_name: &str,
) -> Option<Rendered> {
    let conv: &OAConversationRow = parsed
        .conversations
        .iter()
        .find(|c| c.conversation_id == conversation_id)?;

    let mut msgs_by_conv: HashMap<&str, Vec<&OAMessageRow>> = HashMap::new();
    for m in &parsed.messages {
        msgs_by_conv.entry(&m.conversation_id).or_default().push(m);
    }
    let mut parts_by_msg: HashMap<&str, Vec<&OAContentPartRow>> = HashMap::new();
    for p in &parsed.content_parts {
        parts_by_msg.entry(&p.message_id).or_default().push(p);
    }

    let msgs = msgs_by_conv
        .get(conv.conversation_id.as_str())
        .cloned()
        .unwrap_or_default();
    let msg_by_id: HashMap<&str, &OAMessageRow> =
        msgs.iter().map(|m| (m.message_id.as_str(), *m)).collect();

    // Walk current_node → root via parent_id; fall back to create_time sort.
    let mut path: Vec<&OAMessageRow> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut cursor = conv.current_node.clone();
    while let Some(cid) = cursor {
        if seen.contains(&cid) {
            break;
        }
        let Some(m) = msg_by_id.get(cid.as_str()) else {
            break;
        };
        seen.insert(cid.clone());
        path.push(*m);
        cursor = m.parent_id.clone();
    }
    path.reverse();
    if path.is_empty() {
        let mut sorted: Vec<&OAMessageRow> = msgs.clone();
        sorted.sort_by(|a, b| {
            a.create_time
                .as_deref()
                .unwrap_or("")
                .cmp(b.create_time.as_deref().unwrap_or(""))
        });
        path = sorted;
    }

    let mut out: Vec<String> = Vec::new();
    out.push("---".into());
    out.push("provider: openai".into());
    out.push(format!("id: {}", yaml_scalar(Some(&conv.conversation_id))));
    out.push(format!("title: {}", yaml_scalar(conv.title.as_deref())));
    out.push(format!(
        "account_id: {}",
        yaml_scalar(conv.account_id.as_deref())
    ));
    out.push(format!(
        "create_time: {}",
        yaml_scalar(conv.create_time.as_deref())
    ));
    out.push(format!(
        "update_time: {}",
        yaml_scalar(conv.update_time.as_deref())
    ));
    if let Some(slug) = conv.default_model_slug.as_deref() {
        if !slug.is_empty() {
            out.push(format!("default_model_slug: {}", yaml_scalar(Some(slug))));
        }
    }
    out.push("---".into());
    out.push(String::new());
    out.push(format!(
        "# {}",
        conv.title.as_deref().unwrap_or("(untitled)")
    ));
    out.push(String::new());

    let mut last_ts: Option<String> = conv.create_time.clone();
    for m in &path {
        if m.role.as_deref() == Some("system")
            || m.content_type.as_deref() == Some("model_editable_context")
        {
            continue;
        }
        let mut msg_created = m.create_time.clone();
        if msg_created.is_none() {
            if let Some(prev) = &last_ts {
                msg_created = Some(bump_iso(prev));
            }
        }
        if let Some(ts) = &msg_created {
            last_ts = Some(ts.clone());
        }
        let heading = capitalize(m.role.as_deref().unwrap_or("unknown"));
        out.push(msg_div_open(&m.message_id, "openai"));
        out.push(String::new());
        out.push(format!("## {heading}"));

        let mut meta_bits: Vec<String> = Vec::new();
        if let Some(ts) = &msg_created {
            meta_bits.push(ts.clone());
        }
        if let Some(slug) = m.model_slug.as_deref() {
            if !slug.is_empty() {
                meta_bits.push(slug.into());
            }
        }
        if !meta_bits.is_empty() {
            out.push(String::new());
            out.push(format!("*{}*", meta_bits.join(" · ")));
        }
        out.push(String::new());

        let mut parts = parts_by_msg
            .get(m.message_id.as_str())
            .cloned()
            .unwrap_or_default();
        parts.sort_by_key(|p| p.part_index);
        for p in parts {
            let has_text = p.text.as_deref().is_some_and(|s| !s.is_empty());
            if !has_text && p.kind != "execution_output" && p.kind != "code" {
                continue;
            }
            let anchor = format!("<a id=\"b-{}-{}\"></a>", m.message_id, p.part_index);
            match p.kind.as_str() {
                "text" => {
                    out.push(format!(
                        "{anchor}{}",
                        p.text.as_deref().unwrap_or("").trim_end()
                    ));
                    out.push(String::new());
                }
                "code" => {
                    out.push(anchor);
                    out.push(
                        format!("```{}", p.language.as_deref().unwrap_or(""))
                            .trim_end()
                            .to_string(),
                    );
                    out.push(p.text.as_deref().unwrap_or("").trim_end().into());
                    out.push("```".into());
                    out.push(String::new());
                }
                "execution_output" => {
                    out.push(anchor);
                    out.push("```".into());
                    out.push(p.text.as_deref().unwrap_or("").trim_end().into());
                    out.push("```".into());
                    out.push(String::new());
                }
                "thoughts" | "reasoning_recap" => {
                    out.push(format!("{anchor}<!-- {} -->", p.kind));
                    out.push(format!(
                        "> {}",
                        p.text.as_deref().unwrap_or("").replace('\n', "\n> ")
                    ));
                    out.push(String::new());
                }
                other => {
                    out.push(format!("{anchor}<!-- {other} -->"));
                    out.push(p.text.as_deref().unwrap_or("").trim_end().into());
                    out.push(String::new());
                }
            }
        }
        if !m.attachments.is_empty() {
            for a in &m.attachments {
                out.push(attachment_md(a));
                out.push(String::new());
            }
        }

        out.push(MSG_DIV_CLOSE.into());
        out.push(String::new());
    }

    let mut body = out.join("\n");
    while body.ends_with('\n') || body.ends_with('\r') {
        body.pop();
    }
    body.push('\n');

    Some(Rendered {
        conversation_id: conv.conversation_id.clone(),
        account_id: conv.account_id.clone().unwrap_or_else(|| "unknown".into()),
        body,
    })
}

/// `file_id → file_name` lookup off the attachments we walked during
/// parse, so blob writes pick the same filename `attachment_md`
/// references in the markdown.
fn name_by_id(parsed: &ParsedChatGPTApi) -> HashMap<String, Option<String>> {
    let mut out: HashMap<String, Option<String>> = HashMap::new();
    for m in &parsed.messages {
        for a in &m.attachments {
            out.entry(a.file_id.clone())
                .or_insert_with(|| a.name.clone());
        }
    }
    out
}

/// Write every blob this conversation references into
/// `<page_dir>/blobs/<filename>`. The markdown emitted by
/// `attachment_md` uses the same `safe_filename(name, file_id)` rule
/// so the link target matches what lands on disk. Blobs referenced by
/// other conversations are skipped — they live next to *their*
/// `index.md`.
fn materialize_conv_blobs(
    parsed: &ParsedChatGPTApi,
    conv: &OAConversationRow,
    name_by_id: &HashMap<String, Option<String>>,
    page_dir: &std::path::Path,
) -> std::io::Result<()> {
    if parsed.blobs_by_id.is_empty() {
        return Ok(());
    }
    let mut wanted: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for m in &parsed.messages {
        if m.conversation_id != conv.conversation_id {
            continue;
        }
        for a in &m.attachments {
            wanted.insert(a.file_id.as_str());
        }
    }
    if wanted.is_empty() {
        return Ok(());
    }
    let blobs_dir = page_dir.join("blobs");
    for (file_id, blob) in &parsed.blobs_by_id {
        if !wanted.contains(file_id.as_str()) {
            continue;
        }
        let name = name_by_id.get(file_id.as_str()).and_then(|n| n.as_deref());
        let safe = safe_filename(name, file_id);
        std::fs::create_dir_all(&blobs_dir)?;
        std::fs::write(blobs_dir.join(&safe), &blob.bytes)?;
    }
    Ok(())
}

/// Markdown line for one attachment: `![alt](blobs/<filename>)` for
/// images, `[\[file\] name](blobs/<filename>)` for everything else.
/// The link target is relative to the conversation's `index.md` (the
/// page-dir is `<conv_id>/`, with the blob in `<conv_id>/blobs/`).
fn attachment_md(a: &OAAttachmentRef) -> String {
    let safe = safe_filename(a.name.as_deref(), &a.file_id);
    let alt = a
        .name
        .clone()
        .unwrap_or_else(|| a.file_id.clone())
        .replace(']', "");
    let link = format!("blobs/{safe}");
    if a.is_image {
        format!("![{alt}]({link})")
    } else {
        format!("[\\[file\\] {alt}]({link})")
    }
}
