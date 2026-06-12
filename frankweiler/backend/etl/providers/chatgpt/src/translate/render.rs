//! Port of `_render_one_openai` from `src/ingest/render.py`, plus the
//! shared `_slugify` / `_yaml_scalar` / `_bump_iso` / `_msg_div_open`
//! helpers (kept private here; if the Anthropic crate's copies and
//! these drift, we'll promote them into `frankweiler-etl`).

use std::collections::HashMap;

use anyhow::Context as _;
use frankweiler_etl::blob_cas::{self, BlobReader};
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl::render_cursor;
use frankweiler_etl::title::Title;
use frankweiler_index_lib::emit_sidecar;

use super::grid_rows::{rows_for_conversation, RENDER_VERSION};
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
    let Some(mut out) = frankweiler_time::bump_micros_str(ts, 1) else {
        return ts.to_string();
    };
    if ts.ends_with('Z') && out.ends_with("+00:00") {
        out.truncate(out.len() - 6);
        out.push('Z');
    }
    out
}

use frankweiler_etl::section::{msg_div_open, MSG_DIV_CLOSE};

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
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> anyhow::Result<()>,
) -> anyhow::Result<Vec<std::path::PathBuf>> {
    // Log how long the dolt_diff scan took. Logged on every render
    // (including cold start) so the timing shows up in sync output
    // without users having to crack the cursor open.
    let elapsed_ms = parsed.scan.scan_elapsed.map(|d| d.as_millis() as u64);
    tracing::info!(
        source = source_name,
        scan_elapsed_ms = elapsed_ms,
        changed_conversations = parsed
            .scan
            .changed_conversations
            .as_ref()
            .map(|s| s.len() as i64)
            .unwrap_or(-1),
        cold_start = parsed.scan.changed_conversations.is_none(),
        "[translate] chatgpt dolt_diff scan"
    );

    progress.set_length(Some(
        (parsed.conversations.len() + parsed.docs_skipped) as u64,
    ));
    progress.inc(parsed.docs_skipped as u64);
    let mut written = Vec::new();
    for c in &parsed.conversations {
        let conv_id = &c.conv.conversation_id;
        let account_id = c.conv.account_id.as_deref().unwrap_or("unknown");
        let rel = conv_relative_path(account_id, conv_id);
        let abs = root.join(&rel);
        // The per-doc `source_fingerprint` used to be a hash over the
        // upstream payload. With dolt_diff driving the skip decision
        // upstream of render, that compare is gone; use the
        // conversation_id so the sidecar still has a stable
        // identifier. The orchestrator's prior_fingerprints map is
        // ignored by chatgpt now.
        let fingerprint = conv_id.clone();

        let shredded = shred(c);
        let Some(r) = render_one(&shredded, source_name, parsed.blobs.as_ref()) else {
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
        let sidecar_abs = abs.with_extension("grid_rows.json");
        emit_sidecar(
            &sidecar_abs,
            conv_id,
            &fingerprint,
            RENDER_VERSION,
            &rows,
            &[],
        )
        .map_err(std::io::Error::other)?;

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

    // Advance the render cursor only when everything succeeded AND we
    // managed to read HEAD at scan time. Stock libsqlite3 leaves
    // new_head as None → cursor stays unwritten → next run is another
    // cold start, which is the right behavior since we have no way to
    // anchor the diff.
    if let Some(head) = parsed.scan.new_head.as_deref() {
        let cursor_path = render_cursor::cursor_path(root, "chatgpt", source_name);
        render_cursor::write(&cursor_path, head, parsed.scan.scan_elapsed)
            .with_context(|| format!("write chatgpt render cursor {}", cursor_path.display()))?;
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

pub fn render_one(
    shredded: &ShreddedConversation,
    _source_name: &str,
    blobs: &dyn BlobReader,
) -> Option<Rendered> {
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
                out.push(attachment_md(a, blobs));
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

fn materialize_conv_blobs(
    shredded: &ShreddedConversation,
    blobs: &dyn BlobReader,
    page_dir: &std::path::Path,
) -> std::io::Result<()> {
    let blobs_dir = page_dir.join("blobs");
    blob_cas::materialize_refs(
        blobs,
        shredded
            .messages
            .iter()
            .flat_map(|m| m.attachments.iter().map(|a| a.file_id.as_str())),
        &blobs_dir,
    )
}

fn attachment_md(a: &OAAttachmentRef, blobs: &dyn BlobReader) -> String {
    blob_cas::attachment_md(blobs, &a.file_id, a.name.as_deref(), a.is_image)
}
