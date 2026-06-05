//! Port of `_render_one_openai` from `src/ingest/render.py`, plus the
//! shared `_slugify` / `_yaml_scalar` / `_bump_iso` / `_msg_div_open`
//! helpers (kept private here; if the Anthropic crate's copies and
//! these drift, we'll promote them into `frankweiler-etl`).

use std::collections::HashMap;

use chrono::{DateTime, FixedOffset};

use frankweiler_etl::blob_store::BlobStore;
use frankweiler_etl::blobs::safe_filename;
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl::sidecar::{Sidecar, SidecarHeader};
use frankweiler_etl::title::Title;

use super::grid_rows::{fingerprint_for_conversation, rows_for_conversation, RENDER_VERSION};
use super::parse::{shred, OAAttachmentRef, OAMessageRow, ParsedChatGPTApi, ShreddedConversation};

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
///
/// Per-conversation flow: fingerprint the upstream payload → if the
/// indexer already has that fingerprint and the rendered md is still
/// on disk, skip (and do *not* shred the mapping); otherwise shred,
/// render, write blobs/md/sidecar, hand the `RenderedMarkdown` to the
/// caller's commit callback.
pub fn render_all(
    parsed: &ParsedChatGPTApi,
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
        let conv_id = &c.conv.conversation_id;
        let account_id = c.conv.account_id.as_deref().unwrap_or("unknown");
        let rel = conv_relative_path(account_id, conv_id);
        let abs = root.join(&rel);

        // Skip when the indexer has the same fingerprint AND the md
        // file is still on disk (defends against `rm -rf rendered_md/`
        // by hand). No shredding happens here — the upstream payload
        // alone is enough to decide.
        if prior_fingerprints.get(conv_id).map(String::as_str) == Some(fingerprint.as_str())
            && abs.exists()
        {
            written.push(rel);
            progress.inc(1);
            continue;
        }

        // Changed (or first-time): walk the mapping into messages/parts.
        let shredded = shred(c);
        let Some(r) = render_one(&shredded, source_name) else {
            progress.inc(1);
            continue;
        };

        let page_dir = abs
            .parent()
            .expect("relative_path always has a parent (the page-dir)");
        std::fs::create_dir_all(page_dir)?;

        // Order: blobs → md → sidecar → callback. The callback is the
        // commit point: an interrupted run leaves the indexer
        // un-notified so next run re-tries.
        materialize_conv_blobs(&shredded, parsed.blobs.as_ref(), page_dir)?;

        std::fs::write(&abs, &r.body)?;

        let rows = rows_for_conversation(&shredded);
        let sidecar = Sidecar {
            header: SidecarHeader {
                markdown_uuid: conv_id.clone(),
                source_fingerprint: fingerprint.clone(),
                render_version: RENDER_VERSION,
            },
            rows: rows.clone(),
            edges: Vec::new(),
        };
        let sidecar_abs = abs.with_extension("grid_rows.json");
        let sidecar_json = serde_json::to_string_pretty(&sidecar).map_err(std::io::Error::other)?;
        std::fs::write(&sidecar_abs, sidecar_json)?;

        on_doc_complete(RenderedMarkdown {
            markdown_uuid: conv_id.clone(),
            source_name: source_name.to_string(),
            source_fingerprint: fingerprint,
            upstream_cursor: None,
            md_path: abs.clone(),
            render_version: RENDER_VERSION,
            rows,
            edges: Vec::new(),
        })?;

        written.push(rel);
        progress.inc(1);
    }
    Ok(written)
}

/// Mirror of `Rendered::relative_path` for use *before* we've rendered
/// (so we can fingerprint-skip without paying for a render first).
fn conv_relative_path(account_id: &str, conv_id: &str) -> std::path::PathBuf {
    std::path::PathBuf::from("rendered_md/openai")
        .join(account_id)
        .join("llm_chats")
        .join(conv_id)
        .join("index.md")
}

pub fn render_one(shredded: &ShreddedConversation, _source_name: &str) -> Option<Rendered> {
    let conv = &shredded.conv;
    let msgs: Vec<&OAMessageRow> = shredded.messages.iter().collect();
    let msg_by_id: HashMap<&str, &OAMessageRow> =
        msgs.iter().map(|m| (m.message_id.as_str(), *m)).collect();
    let mut parts_by_msg: HashMap<&str, Vec<&super::parse::OAContentPartRow>> = HashMap::new();
    for p in &shredded.content_parts {
        parts_by_msg.entry(&p.message_id).or_default().push(p);
    }

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
    let source_url = format!("https://chatgpt.com/c/{}", conv.conversation_id);
    let title_block = Title {
        text: conv.title.as_deref().unwrap_or("(untitled)"),
        markdown_uuid: Some(&conv.conversation_id),
        source_url: Some(&source_url),
    }
    .render();
    // `render()` ends with `\n\n`; we're about to `out.join("\n")`, so
    // strip the trailing newlines and push a single String — the
    // following blank line restores paragraph separation.
    out.push(title_block.trim_end().to_string());
    out.push(String::new());

    let mut last_ts: Option<String> = conv.create_time.clone();
    for m in &path {
        // Render every message in the path, including `system` /
        // `model_editable_context` rows. The grid_rows sidecar emits
        // one row per message (so search and the grid index can
        // surface them), so the renderer has to keep them in sync —
        // otherwise the grid links to a section that doesn't exist
        // in the preview pane, looking like data loss. If we ever
        // want to hide them, do it visually (collapsible block /
        // de-emphasized styling), not by dropping them on the floor
        // here. Same goes for `rows_for_conversation` in
        // grid_rows.rs: keep both sides emitting the same set.
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

/// Write every blob this conversation references into
/// `<page_dir>/blobs/<filename>`. The markdown emitted by
/// `attachment_md` uses the same `safe_filename(name, file_id)` rule
/// so the link target matches what lands on disk.
fn materialize_conv_blobs(
    shredded: &ShreddedConversation,
    blobs: &dyn BlobStore,
    page_dir: &std::path::Path,
) -> std::io::Result<()> {
    // file_id → first name we saw for it, walking only this
    // conversation's attachments. Each conversation owns its blobs in
    // a sibling `blobs/` dir, so there's no need to cross-reference
    // other conversations.
    let mut name_by_id: HashMap<&str, Option<&str>> = HashMap::new();
    for m in &shredded.messages {
        for a in &m.attachments {
            name_by_id
                .entry(a.file_id.as_str())
                .or_insert(a.name.as_deref());
        }
    }
    if name_by_id.is_empty() {
        return Ok(());
    }
    let blobs_dir = page_dir.join("blobs");
    for (file_id, name) in &name_by_id {
        let blob = match blobs.read_by_id(file_id) {
            Ok(Some(b)) => b,
            Ok(None) => continue,
            Err(e) => return Err(std::io::Error::other(e)),
        };
        let safe = safe_filename(*name, file_id);
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
    // `safe_filename` keeps spaces and other readable characters
    // (e.g. `Screenshot 2026-05-13 at 21.16.40.png`), but markdown
    // link targets can't contain raw spaces or parens — percent-encode
    // the offenders so the link still resolves.
    let link = format!("blobs/{}", encode_link_path(&safe));
    if a.is_image {
        format!("![{alt}]({link})")
    } else {
        format!("[\\[file\\] {alt}]({link})")
    }
}

fn encode_link_path(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            ' ' => out.push_str("%20"),
            '(' => out.push_str("%28"),
            ')' => out.push_str("%29"),
            '?' => out.push_str("%3F"),
            '#' => out.push_str("%23"),
            other => out.push(other),
        }
    }
    out
}
