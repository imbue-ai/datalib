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

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl::render_cursor;
use frankweiler_etl::section::section_attrs;
use frankweiler_etl::title::Title;
use frankweiler_index_lib::emit_sidecar;
use frankweiler_schema::grid_rows::GridRow;
use frankweiler_time::IsoOffsetTimestamp;

use super::parse::{DocBucket, ParsedChat, ParsedChatItem, ParsedSignal};
use super::{signal_chat_uuid, signal_markdown_uuid, signal_message_uuid};

/// Bump when the rendered markdown / grid_rows layout changes enough
/// that we need every existing doc rebuilt.
///
/// v3 = each chat item now emits `id="m-{msg_uuid}"
/// data-section-uuid="{msg_uuid}"` on its inline span, so the
/// frontend's row-click → preview-scroll path can anchor on the
/// message_grid_row uuid. Without this the chat-preview pane has no
/// hook for `scrollIntoView` and same-thread row clicks were silently
/// no-op (caught by `row-click-scroll-position.spec.ts`).
///
/// v2 = period-bucketed (one .md per (chat, period_key) instead of one per chat).
pub const RENDER_VERSION: u32 = 3;

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
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    // Log how long the dolt_diff scan took. Logged on every render
    // (including cold start with `None`) so the timing shows up in
    // sync output without the user having to crack the cursor open.
    let elapsed_ms = parsed.scan.scan_elapsed.map(|d| d.as_millis() as u64);
    tracing::info!(
        source = source_name,
        scan_elapsed_ms = elapsed_ms,
        changed_chats = parsed
            .scan
            .changed_chats
            .as_ref()
            .map(|s| s.len() as i64)
            .unwrap_or(-1),
        cold_start = parsed.scan.changed_chats.is_none(),
        "[translate] signal dolt_diff scan"
    );

    let mut summary = RenderSummary {
        // `docs` only contains the buckets that need re-rendering,
        // so `docs_total = parsed.docs.len() + docs_skipped` is the
        // count the orchestrator's progress bar wants. Skip count
        // comes from parse — it counted chats whose dolt_diff entry
        // was empty.
        docs_total: parsed.docs.len() + parsed.docs_skipped,
        docs_skipped: parsed.docs_skipped,
        ..Default::default()
    };
    progress.set_length(Some(summary.docs_total as u64));
    // The parse-side skip-load has already filtered out unchanged
    // buckets; report them all up front so the progress bar accounts
    // for them too.
    progress.inc(summary.docs_skipped as u64);

    for doc in &parsed.docs {
        let Some(chat) = parsed.chats.get(&doc.chat_id) else {
            tracing::warn!(
                event = "signal_render_missing_chat",
                chat_id = %doc.chat_id,
                period_key = %doc.period_key,
            );
            progress.inc(1);
            continue;
        };
        let RenderOutcome::Rendered { messages, blobs } =
            render_one(chat, doc, parsed, out_dir, source_name, on_doc_complete)?;
        summary.docs_rendered += 1;
        summary.messages_rendered += messages;
        summary.blobs_materialized += blobs;
        progress.inc(1);
    }

    // Advance the render cursor only when:
    //   * every doc rendered without error (we're here, so true), AND
    //   * we managed to read HEAD at scan time.
    // A missing HEAD (stock libsqlite3 / non-doltlite db) leaves the
    // cursor unwritten — next run is another cold start, which is the
    // right behavior since we have no way to anchor the diff.
    if let Some(head) = parsed.scan.new_head.as_deref() {
        let cursor_path = render_cursor::cursor_path(out_dir, "signal", source_name);
        render_cursor::write(&cursor_path, head, parsed.scan.scan_elapsed)
            .with_context(|| format!("write signal render cursor {}", cursor_path.display()))?;
    }
    Ok(summary)
}

enum RenderOutcome {
    Rendered { messages: usize, blobs: usize },
}

fn render_one(
    chat: &ParsedChat,
    doc: &DocBucket,
    parsed: &ParsedSignal,
    out_dir: &Path,
    source_name: &str,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderOutcome> {
    let chat_uuid = signal_chat_uuid(source_name, &chat.id);
    let markdown_uuid = signal_markdown_uuid(&chat_uuid, &doc.period_key);
    // The per-doc `source_fingerprint` used to be a content hash
    // computed by the parse-side bucket-fingerprint CTE. With
    // dolt_diff driving the skip decision, that compare doesn't
    // happen anymore — but the load path still wants *some* stable
    // identifier in the sidecar. Use the markdown_uuid: stable across
    // re-renders of the same bucket, distinct between buckets,
    // already in scope. The orchestrator's prior_fingerprints map is
    // ignored by signal now (parse never reads it); this value just
    // keeps the sidecar schema honest.
    let fingerprint = markdown_uuid.clone();

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

    // No prior-fingerprint check here: parse already filtered out
    // unchanged buckets before they reach render. Reaching this
    // function means we have committed to writing this doc.
    fs::create_dir_all(&page_dir).with_context(|| format!("mkdir -p {}", page_dir.display()))?;

    // Materialize attachment bytes into `<page_dir>/blobs/<short-b3>.<ext>`
    // before the .md is written, so the relative links the renderer
    // emits resolve to files that exist by the time the file appears
    // on disk. Filename comes from `Blob::rendered_filename` — same
    // convention every other provider uses (slack, anthropic, notion,
    // chatgpt, email), via the shared `BlobBundle::materialize_to_dir`.
    let blobs_dir = page_dir.join("blobs");
    let ref_ids: Vec<&str> = doc
        .items
        .iter()
        .flat_map(|it| it.attachments.iter().map(|a| a.ref_id.as_str()))
        .collect();
    let mut blobs_written = 0usize;
    if !ref_ids.is_empty() {
        doc.blobs
            .materialize_to_dir(&blobs_dir)
            .with_context(|| format!("materialize blobs into {}", blobs_dir.display()))?;
        // Count what landed on disk so the summary matches what we
        // wrote. The bundle lookup is sync now.
        for ref_id in &ref_ids {
            if doc.blobs.get(ref_id).is_some() {
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
        source_name,
        &chat.id,
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

    emit_sidecar(
        &json_path,
        &markdown_uuid,
        &fingerprint,
        RENDER_VERSION,
        &rows,
        &[],
    )?;

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

#[allow(clippy::too_many_arguments)]
fn render_markdown(
    doc: &DocBucket,
    parsed: &ParsedSignal,
    source_name: &str,
    chat_id: &str,
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
    // Use the shared `Title` helper so signal pages carry the same
    // `class="page-title" data-page-title-uuid="…"` hook the Vue side
    // keys the "copy page ID" button off. Signal Android backups don't
    // expose a per-thread web URL, so `source_url` stays `None` —
    // produces an H1 with the copy-id hook and no outbound ↗ arrow.
    s.push_str(
        &Title {
            text: title,
            markdown_uuid: Some(markdown_uuid),
            source_url: None,
        }
        .render(),
    );

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
        let msg_uuid = signal_message_uuid(source_name, chat_id, &item.author_id, item.date_sent);
        let author = author_display(parsed, item);
        let ts = iso_ts(item.date_sent);
        let body = item.text.as_deref().unwrap_or("");
        // Shared `section_attrs` produces the same `id="m-{uuid}"
        // data-section-uuid="{uuid}"` fragment every other provider
        // emits on its message wrapper; signal puts it on a bullet
        // span instead of a `<div>` so the markdown stays a tight
        // bulleted list. The span wraps visible content (timestamp +
        // author) so the browser gives it non-zero size — an empty
        // anchor span would be `hidden` to the frontend's selection
        // highlighting and Playwright's `toBeVisible` check.
        s.push_str(&format!(
            "- <span {attrs} data-msg-index=\"{idx}\">**{ts}** _{author}_:</span> {body}\n",
            attrs = section_attrs(&msg_uuid),
        ));
        // One sub-bullet per attachment so multi-attachment messages
        // stay readable. The bundle gives us the same image-vs-file
        // split with a "(not yet fetched)" placeholder when the bytes
        // haven't been ingested.
        for att in &item.attachments {
            let link = doc
                .blobs
                .markdown_link(&att.ref_id, att.file_name.as_deref(), att.is_image);
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
        Some(when_ts.to_string()),
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
        Some(when_ts.to_string()),
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
    when_ts: Option<String>,
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
    IsoOffsetTimestamp::from_unix_millis(date_sent_ms)
        .map(|t| t.to_rfc3339_secs())
        .unwrap_or_else(|| {
            // Out-of-range epoch ms (chrono covers ±580B years) is
            // unreachable in practice for Signal payloads. Preserve a
            // sortable epoch as the visible-broken fallback — and warn
            // loudly so the row gets noticed. See data_architecture_ingestion.md
            // "no fabricated timestamps".
            tracing::warn!(
                date_sent_ms,
                "signal::iso_ts: date_sent_ms out of chrono range; fallback used"
            );
            "1970-01-01T00:00:00+00:00".to_string()
        })
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
