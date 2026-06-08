//! Markdown + grid_rows rendering for Signal chats.
//!
//! One `.md` per chat. Each chat item becomes a single bullet:
//!
//! ```text
//! - 2364-04-09 12:00:00 UTC  Picard: Status report.
//! - 2364-04-09 12:01:00 UTC  Riker:  All decks at green status, Captain.
//! ```
//!
//! Sidecar carries: one chat-level grid_row (kind `Signal Chat`) plus
//! one message-level grid_row per chat item (kind `Signal Message`)
//! that surfaces in the search grid.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_schema::grid_rows::GridRow;
use sha2::{Digest, Sha256};

use super::parse::{ParsedChat, ParsedChatItem, ParsedSignal};
use super::{signal_chat_uuid, signal_message_uuid};

/// Bump when the rendered markdown / grid_rows layout changes enough
/// that we need every existing doc rebuilt.
pub const RENDER_VERSION: u32 = 1;

const SOURCE_LABEL: &str = "Signal";
const PROVIDER: &str = "signal";

#[derive(Debug, Default, Clone)]
pub struct RenderSummary {
    pub chats_total: usize,
    pub chats_rendered: usize,
    pub chats_skipped: usize,
    pub messages_rendered: usize,
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
        chats_total: parsed.chats.len(),
        ..Default::default()
    };
    progress.set_length(Some(summary.chats_total as u64));

    for chat in &parsed.chats {
        let outcome = render_one(
            chat,
            parsed,
            out_dir,
            source_name,
            prior_fingerprints,
            on_doc_complete,
        )?;
        match outcome {
            RenderOutcome::Rendered { messages } => {
                summary.chats_rendered += 1;
                summary.messages_rendered += messages;
            }
            RenderOutcome::Skipped => summary.chats_skipped += 1,
        }
        progress.inc(1);
    }
    Ok(summary)
}

enum RenderOutcome {
    Rendered { messages: usize },
    Skipped,
}

fn render_one(
    chat: &ParsedChat,
    parsed: &ParsedSignal,
    out_dir: &Path,
    source_name: &str,
    prior_fingerprints: &HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderOutcome> {
    let chat_uuid = signal_chat_uuid(source_name, &chat.id);
    let fingerprint = compute_fingerprint(chat);

    let recipient_display = parsed
        .recipients
        .get(&chat.recipient_id)
        .map(|r| r.display())
        .unwrap_or_else(|| format!("recipient_{}", chat.recipient_id));
    let title = format!("Signal · {recipient_display}");
    let (md_path, json_path, page_dir) = output_paths(
        out_dir,
        source_name,
        &chat.id,
        &recipient_display,
        &chat_uuid,
    );

    if prior_fingerprints.get(&chat_uuid).map(String::as_str) == Some(fingerprint.as_str())
        && md_path.exists()
    {
        return Ok(RenderOutcome::Skipped);
    }
    fs::create_dir_all(&page_dir).with_context(|| format!("mkdir -p {}", page_dir.display()))?;

    let when_ts = chat
        .items
        .last()
        .map(|i| iso_ts(i.date_sent))
        .unwrap_or_else(|| iso_ts(0));

    let md = render_markdown(
        chat,
        parsed,
        &title,
        &recipient_display,
        &chat_uuid,
        &fingerprint,
    );
    fs::write(&md_path, md).with_context(|| format!("write {}", md_path.display()))?;

    let md_rel_path = md_path
        .strip_prefix(out_dir)
        .unwrap_or(&md_path)
        .to_string_lossy()
        .into_owned();

    // Chat-level row.
    let mut rows: Vec<GridRow> = Vec::with_capacity(1 + chat.items.len());
    rows.push(chat_grid_row(
        &chat_uuid,
        &title,
        &recipient_display,
        &when_ts,
        &md_rel_path,
    ));

    // One message-level row per chat item.
    let mut messages_rendered = 0;
    for (idx, item) in chat.items.iter().enumerate() {
        let Some(text) = item.text.as_deref() else {
            continue;
        };
        let msg_uuid = signal_message_uuid(source_name, &chat.id, &item.author_id, item.date_sent);
        let author = author_display(parsed, item);
        rows.push(message_grid_row(
            &msg_uuid,
            &chat_uuid,
            &title,
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
            "markdown_uuid": chat_uuid,
            "source_fingerprint": fingerprint,
            "render_version": RENDER_VERSION,
        },
        "rows": &rows,
    });
    fs::write(&json_path, serde_json::to_string_pretty(&sidecar)?)
        .with_context(|| format!("write {}", json_path.display()))?;

    on_doc_complete(RenderedMarkdown {
        markdown_uuid: chat_uuid.clone(),
        source_name: source_name.to_string(),
        source_fingerprint: fingerprint,
        upstream_cursor: None,
        md_path,
        render_version: RENDER_VERSION,
        rows,
        edges: Vec::new(),
    })
    .with_context(|| format!("on_doc_complete {chat_uuid}"))?;

    Ok(RenderOutcome::Rendered {
        messages: messages_rendered,
    })
}

fn output_paths(
    out_dir: &Path,
    source_name: &str,
    chat_id: &str,
    recipient_display: &str,
    chat_uuid: &str,
) -> (PathBuf, PathBuf, PathBuf) {
    let page_dir = out_dir.join("rendered_md").join("signal").join(source_name);
    let slug = format!(
        "chat-{chat_id}__{slug}__{short}",
        slug = slugify(recipient_display),
        short = &chat_uuid[..8],
    );
    let md_path = page_dir.join(format!("{slug}.md"));
    let json_path = page_dir.join(format!("{slug}.grid_rows.json"));
    (md_path, json_path, page_dir)
}

fn render_markdown(
    chat: &ParsedChat,
    parsed: &ParsedSignal,
    title: &str,
    recipient_display: &str,
    chat_uuid: &str,
    fingerprint: &str,
) -> String {
    let mut s = String::new();
    s.push_str("---\n");
    s.push_str(&format!("title: \"{}\"\n", title.replace('"', "\\\"")));
    s.push_str(&format!("provider: {PROVIDER}\n"));
    s.push_str(&format!("chat_uuid: {chat_uuid}\n"));
    s.push_str(&format!(
        "recipient: \"{}\"\n",
        recipient_display.replace('"', "\\\"")
    ));
    s.push_str(&format!("source_fingerprint: {fingerprint}\n"));
    s.push_str("---\n\n");
    s.push_str(&format!("# {title}\n\n"));

    if chat.items.is_empty() {
        s.push_str("_(no messages)_\n");
        return s;
    }
    for (idx, item) in chat.items.iter().enumerate() {
        let Some(text) = item.text.as_deref() else {
            continue;
        };
        let author = author_display(parsed, item);
        let ts = iso_ts(item.date_sent);
        s.push_str(&format!(
            "- <span data-msg-index=\"{idx}\"></span>**{ts}** _{author}_: {text}\n"
        ));
    }
    s
}

fn chat_grid_row(
    chat_uuid: &str,
    title: &str,
    recipient_display: &str,
    when_ts: &str,
    qmd_rel: &str,
) -> GridRow {
    base_row(
        chat_uuid.to_string(),
        "Signal Chat".to_string(),
        title.to_string(),
        Some(recipient_display.to_string()),
        chat_uuid.to_string(),
        None,
        when_ts.to_string(),
        title.to_string(),
        qmd_rel.to_string(),
    )
}

#[allow(clippy::too_many_arguments)]
fn message_grid_row(
    msg_uuid: &str,
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
) -> GridRow {
    GridRow {
        uuid: uuid.clone(),
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
        entire_chat: format!("/chat/{conversation_uuid}"),
        text,
        slack_link: None,
        qmd_path: Some(qmd_path),
        source_url: None,
        git_sha: None,
        external_id: None,
        notion_page_uuid: None,
        notion_block_uuid: None,
        markdown_uuid: Some(conversation_uuid),
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
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true))
        .unwrap_or_else(|| "1970-01-01T00:00:00Z".to_string())
}

fn compute_fingerprint(chat: &ParsedChat) -> String {
    let mut h = Sha256::new();
    h.update(chat.id.as_bytes());
    h.update(b"|");
    h.update(chat.recipient_id.as_bytes());
    for item in &chat.items {
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
