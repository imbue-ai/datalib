//! Markdown + grid_rows rendering for Signal chats.
//!
//! One `.md` per `(chat, period_key)` bucket. Layout under `out_dir`:
//!
//! ```text
//! rendered_md/signal/<source_name>/<chat-slug>/<period_key>.md
//! rendered_md/signal/<source_name>/<chat-slug>/<period_key>.grid_rows.json
//! ```
//!
//! Each chat item in a bucket becomes one line of the markdown body:
//!
//! ```text
//! - 2364-04-09T12:00:00Z  Me: Status report.
//! - 2364-04-09T12:01:00Z  Will Riker: All decks at green status, Captain.
//! ```
//!
//! Sidecar carries: one chat-level grid_row (`Signal Chat`) per
//! bucket plus one message-level grid_row (`Signal Message`) per
//! chat item that surfaces in the search grid.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{SecondsFormat, TimeZone, Utc};
use frankweiler_etl::blob_cas;
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_schema::grid_rows::GridRow;
use sha2::{Digest, Sha256};

use super::parse::{DocBucket, ParsedChat, ParsedChatItem, ParsedSignal};
use super::{signal_chat_uuid, signal_markdown_uuid, signal_message_uuid};

/// Bump when the rendered markdown / grid_rows layout changes enough
/// that we need every existing doc rebuilt. v2 = period-bucketed
/// (one .md per (chat, period_key) instead of one per chat).
pub const RENDER_VERSION: u32 = 2;

const SOURCE_LABEL: &str = "Signal";
const PROVIDER: &str = "signal";

#[derive(Debug, Default, Clone)]
pub struct RenderSummary {
    pub docs_total: usize,
    pub docs_rendered: usize,
    pub docs_skipped: usize,
    pub messages_rendered: usize,
    /// Attachment blobs materialized onto disk under
    /// `<page_dir>/blobs/<short-b3>.<ext>`.
    pub blobs_materialized: usize,
}

pub fn render_all(
    parsed: &ParsedSignal,
    out_dir: &Path,
    source_name: &str,
    progress: &Progress,
    prior_fingerprints: &HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    let mut summary = RenderSummary {
        docs_total: parsed.docs.len(),
        ..Default::default()
    };
    progress.set_length(Some(summary.docs_total as u64));

    for doc in &parsed.docs {
        let Some(chat) = parsed.chats.get(&doc.chat_id) else {
            // Parser populates the chat map from the same db, so this
            // is a "shouldn't happen" path — log and skip rather than
            // abort the whole translate pass.
            tracing::warn!(
                event = "signal_render_missing_chat",
                chat_id = %doc.chat_id,
                period_key = %doc.period_key,
            );
            progress.inc(1);
            continue;
        };
        let outcome = render_one(
            chat,
            doc,
            parsed,
            out_dir,
            source_name,
            prior_fingerprints,
            on_doc_complete,
        )?;
        match outcome {
            RenderOutcome::Rendered { messages, blobs } => {
                summary.docs_rendered += 1;
                summary.messages_rendered += messages;
                summary.blobs_materialized += blobs;
            }
            RenderOutcome::Skipped => summary.docs_skipped += 1,
        }
        progress.inc(1);
    }
    Ok(summary)
}

enum RenderOutcome {
    Rendered { messages: usize, blobs: usize },
    Skipped,
}

fn render_one(
    chat: &ParsedChat,
    doc: &DocBucket,
    parsed: &ParsedSignal,
    out_dir: &Path,
    source_name: &str,
    prior_fingerprints: &HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderOutcome> {
    let chat_uuid = signal_chat_uuid(source_name, &chat.id);
    let markdown_uuid = signal_markdown_uuid(&chat_uuid, &doc.period_key);
    let fingerprint = compute_fingerprint(doc);

    let recipient_display = parsed
        .recipients
        .get(&chat.recipient_id)
        .map(|r| r.display())
        .unwrap_or_else(|| format!("recipient_{}", chat.recipient_id));
    let chat_title = format!("Signal · {recipient_display}");
    let doc_title = format!("{chat_title} ({})", doc.period_key);

    let (md_path, json_path, page_dir) = output_paths(
        out_dir,
        source_name,
        &chat.id,
        &recipient_display,
        &chat_uuid,
        &doc.period_key,
    );

    if prior_fingerprints.get(&markdown_uuid).map(String::as_str) == Some(fingerprint.as_str())
        && md_path.exists()
    {
        return Ok(RenderOutcome::Skipped);
    }
    fs::create_dir_all(&page_dir).with_context(|| format!("mkdir -p {}", page_dir.display()))?;

    // Materialize attachment bytes into `<page_dir>/blobs/<short-b3>.<ext>`
    // before the .md is written, so the relative links the renderer
    // emits resolve to files that exist by the time the file appears
    // on disk. Filename comes from `BlobView::rendered_filename` —
    // same convention every other provider uses (slack, anthropic,
    // notion, chatgpt, email), via the universal helper.
    let blobs_dir = page_dir.join("blobs");
    let mut blobs_written = 0usize;
    let ref_ids: Vec<&str> = doc
        .items
        .iter()
        .flat_map(|it| it.attachments.iter().map(|a| a.ref_id.as_str()))
        .collect();
    if !ref_ids.is_empty() {
        blob_cas::materialize_refs(parsed.blobs.as_ref(), ref_ids.iter().copied(), &blobs_dir)
            .with_context(|| format!("materialize blobs into {}", blobs_dir.display()))?;
        // Count what landed on disk so the summary matches what we wrote.
        for ref_id in &ref_ids {
            if parsed.blobs.read_by_ref_id(ref_id).ok().flatten().is_some() {
                blobs_written += 1;
            }
        }
    }

    let when_ts = doc
        .items
        .last()
        .map(|i| iso_ts(i.date_sent))
        .unwrap_or_else(|| iso_ts(0));

    let md = render_markdown(
        doc,
        parsed,
        &doc_title,
        &recipient_display,
        &chat_uuid,
        &markdown_uuid,
        &fingerprint,
    );
    fs::write(&md_path, md).with_context(|| format!("write {}", md_path.display()))?;

    let md_rel_path = md_path
        .strip_prefix(out_dir)
        .unwrap_or(&md_path)
        .to_string_lossy()
        .into_owned();

    let mut rows: Vec<GridRow> = Vec::with_capacity(1 + doc.items.len());
    rows.push(chat_grid_row(
        &markdown_uuid,
        &chat_uuid,
        &doc_title,
        &recipient_display,
        &when_ts,
        &md_rel_path,
    ));

    let mut messages_rendered = 0;
    for (idx, item) in doc.items.iter().enumerate() {
        let Some(text) = item.text.as_deref() else {
            continue;
        };
        let msg_uuid = signal_message_uuid(source_name, &chat.id, &item.author_id, item.date_sent);
        let author = author_display(parsed, item);
        rows.push(message_grid_row(
            &msg_uuid,
            &markdown_uuid,
            &chat_uuid,
            &chat_title,
            &author,
            text,
            idx as i64,
            &iso_ts(item.date_sent),
            &md_rel_path,
        ));
        messages_rendered += 1;
    }

    let sidecar = serde_json::json!({
        "header": {
            "markdown_uuid": markdown_uuid,
            "source_fingerprint": fingerprint,
            "render_version": RENDER_VERSION,
        },
        "rows": &rows,
    });
    fs::write(&json_path, serde_json::to_string_pretty(&sidecar)?)
        .with_context(|| format!("write {}", json_path.display()))?;

    on_doc_complete(RenderedMarkdown {
        markdown_uuid: markdown_uuid.clone(),
        source_name: source_name.to_string(),
        source_fingerprint: fingerprint,
        upstream_cursor: None,
        md_path,
        render_version: RENDER_VERSION,
        rows,
        edges: Vec::new(),
    })
    .with_context(|| format!("on_doc_complete {markdown_uuid}"))?;

    Ok(RenderOutcome::Rendered {
        messages: messages_rendered,
        blobs: blobs_written,
    })
}

fn output_paths(
    out_dir: &Path,
    source_name: &str,
    chat_id: &str,
    recipient_display: &str,
    chat_uuid: &str,
    period_key: &str,
) -> (PathBuf, PathBuf, PathBuf) {
    // One directory per chat, with period_key files inside —
    // mirrors beeper's `<network>/<room_uuid>/<period_key>.md` shape.
    let chat_slug = format!(
        "chat-{chat_id}__{slug}__{short}",
        slug = slugify(recipient_display),
        short = &chat_uuid[..8],
    );
    let page_dir = out_dir
        .join("rendered_md")
        .join("signal")
        .join(source_name)
        .join(&chat_slug);
    let md_path = page_dir.join(format!("{period_key}.md"));
    let json_path = page_dir.join(format!("{period_key}.grid_rows.json"));
    (md_path, json_path, page_dir)
}

fn render_markdown(
    doc: &DocBucket,
    parsed: &ParsedSignal,
    title: &str,
    recipient_display: &str,
    chat_uuid: &str,
    markdown_uuid: &str,
    fingerprint: &str,
) -> String {
    let mut s = String::new();
    s.push_str("---\n");
    s.push_str(&format!("title: \"{}\"\n", title.replace('"', "\\\"")));
    s.push_str(&format!("provider: {PROVIDER}\n"));
    s.push_str(&format!("chat_uuid: {chat_uuid}\n"));
    s.push_str(&format!("markdown_uuid: {markdown_uuid}\n"));
    s.push_str(&format!("period: {}\n", doc.period_key));
    s.push_str(&format!(
        "recipient: \"{}\"\n",
        recipient_display.replace('"', "\\\"")
    ));
    s.push_str(&format!("source_fingerprint: {fingerprint}\n"));
    s.push_str("---\n\n");
    s.push_str(&format!("# {title}\n\n"));

    if doc.items.is_empty() {
        s.push_str("_(no messages)_\n");
        return s;
    }
    for (idx, item) in doc.items.iter().enumerate() {
        // Skip the bullet entirely only when there's neither text
        // nor an attachment — that's a non-rendering ChatItem
        // (StickerMessage, ChatUpdate, …) that contributes nothing
        // to a chat-replay markdown.
        if item.text.is_none() && item.attachments.is_empty() {
            continue;
        }
        let author = author_display(parsed, item);
        let ts = iso_ts(item.date_sent);
        let body = item.text.as_deref().unwrap_or("");
        s.push_str(&format!(
            "- <span data-msg-index=\"{idx}\"></span>**{ts}** _{author}_: {body}\n"
        ));
        // One sub-bullet per attachment so multi-attachment messages
        // stay readable. The link target is generated by the universal
        // `blob_cas::attachment_md` helper — same image-vs-file split
        // every other provider uses, with a "(not yet fetched)"
        // placeholder when the bytes haven't been ingested.
        for att in &item.attachments {
            let link = blob_cas::attachment_md(
                parsed.blobs.as_ref(),
                &att.ref_id,
                att.file_name.as_deref(),
                att.is_image,
            );
            s.push_str("    - ");
            s.push_str(&link);
            s.push('\n');
        }
    }
    s
}

fn chat_grid_row(
    markdown_uuid: &str,
    chat_uuid: &str,
    title: &str,
    recipient_display: &str,
    when_ts: &str,
    qmd_rel: &str,
) -> GridRow {
    base_row(
        markdown_uuid.to_string(),
        "Signal Chat".to_string(),
        title.to_string(),
        Some(recipient_display.to_string()),
        chat_uuid.to_string(),
        None,
        when_ts.to_string(),
        title.to_string(),
        qmd_rel.to_string(),
        markdown_uuid.to_string(),
    )
}

#[allow(clippy::too_many_arguments)]
fn message_grid_row(
    msg_uuid: &str,
    markdown_uuid: &str,
    chat_uuid: &str,
    title: &str,
    author: &str,
    text: &str,
    idx: i64,
    when_ts: &str,
    qmd_rel: &str,
) -> GridRow {
    base_row(
        msg_uuid.to_string(),
        "Signal Message".to_string(),
        text.to_string(),
        Some(author.to_string()),
        chat_uuid.to_string(),
        Some(idx),
        when_ts.to_string(),
        title.to_string(),
        qmd_rel.to_string(),
        markdown_uuid.to_string(),
    )
}

#[allow(clippy::too_many_arguments)]
fn base_row(
    uuid: String,
    kind: String,
    text: String,
    author: Option<String>,
    conversation_uuid: String,
    message_index: Option<i64>,
    when_ts: String,
    conversation_name: String,
    qmd_path: String,
    markdown_uuid: String,
) -> GridRow {
    GridRow {
        uuid,
        provider: PROVIDER.to_string(),
        kind,
        source_label: SOURCE_LABEL.to_string(),
        when_ts,
        author,
        account: None,
        project: None,
        org_uuid: None,
        org_name: None,
        channel: None,
        conversation_name: Some(conversation_name),
        conversation_uuid: conversation_uuid.clone(),
        message_index,
        entire_chat: format!("/chat/{markdown_uuid}"),
        text,
        slack_link: None,
        qmd_path: Some(qmd_path),
        source_url: None,
        git_sha: None,
        external_id: None,
        notion_page_uuid: None,
        notion_block_uuid: None,
        markdown_uuid: Some(markdown_uuid),
    }
}

fn author_display(parsed: &ParsedSignal, item: &ParsedChatItem) -> String {
    if item.outgoing {
        return "Me".to_string();
    }
    parsed
        .recipients
        .get(&item.author_id)
        .map(|r| r.display())
        .unwrap_or_else(|| format!("recipient_{}", item.author_id))
}

fn iso_ts(date_sent_ms: i64) -> String {
    Utc.timestamp_millis_opt(date_sent_ms)
        .single()
        .map(|t| t.to_rfc3339_opts(SecondsFormat::Secs, true))
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
}

fn compute_fingerprint(doc: &DocBucket) -> String {
    let mut h = Sha256::new();
    h.update(doc.chat_id.as_bytes());
    h.update(b"|");
    h.update(doc.period_key.as_bytes());
    for item in &doc.items {
        h.update(b"\n");
        h.update(item.author_id.as_bytes());
        h.update(b"|");
        h.update(item.date_sent.to_string().as_bytes());
        h.update(b"|");
        h.update(item.outgoing.to_string().as_bytes());
        h.update(b"|");
        h.update(item.text.as_deref().unwrap_or("").as_bytes());
    }
    format!("{:x}", h.finalize())
}

fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut last_dash = true;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    out.trim_matches('-').to_string()
}
