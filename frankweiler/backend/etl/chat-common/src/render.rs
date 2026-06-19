//! `render_all` — drives every `(chat, period)` bucket through
//! [`render_one`], handles fingerprint-skip, and feeds rendered docs
//! into the orchestrator's `on_doc_complete` callback.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use frankweiler_etl::blob_cas::BlobBundle;
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl::section::msg_div_open;
use frankweiler_etl::title::Title;
use frankweiler_index_lib::emit_sidecar;
use frankweiler_schema::grid_rows::GridRow;
use frankweiler_time::IsoOffsetTimestamp;
use sha2::{Digest, Sha256};

use crate::types::{ItemKind, NormalizedChat, NormalizedChatItem, NormalizedDoc};

/// Per-provider knobs the renderer parameterizes on. Values that
/// would otherwise be hard-coded as `"signal"` / `"Signal Chat"` /
/// `"Signal Message"` so a single render function serves every chat
/// provider.
#[derive(Debug, Clone)]
pub struct RenderProfile {
    /// On-disk subdir under `rendered_md/<provider>/<source_name>/…`
    /// and the value of the markdown's `provider:` frontmatter key.
    pub provider: &'static str,
    /// The `source_label` column on every grid_row this provider
    /// emits. Beeper sets this to a composite like `"Beeper:Signal"`;
    /// Signal/WhatsApp set it to plain `"Signal"` / `"WhatsApp"`.
    pub source_label: String,
    /// Discriminator for chat-level grid_rows (e.g. `"Signal Chat"`,
    /// `"WhatsApp Chat"`, `"Beeper Signal Chat"`).
    pub chat_kind: String,
    /// Discriminator for message-level grid_rows.
    pub message_kind: String,
    /// Discriminator for reaction-level grid_rows. Reactions get their
    /// own rows so search can find them by emoji content.
    pub reaction_kind: String,
    /// Each provider bumps its own render version when its translate
    /// layer changes meaningfully (column changes, item-shape changes,
    /// new field on grid_rows). The chat-common renderer stamps this
    /// into the sidecar so a re-run knows to invalidate stale docs.
    pub render_version: u32,
}

#[derive(Debug, Default, Clone)]
pub struct RenderSummary {
    pub docs_total: usize,
    pub docs_rendered: usize,
    pub docs_skipped: usize,
    pub items_rendered: usize,
    pub reactions_rendered: usize,
}

/// Render every bucket of every chat. Returns aggregate counts; the
/// per-doc work is delegated to [`render_one`].
///
/// `blobs_by_chat` maps `chat.id` to the per-chat
/// [`BlobBundle`](frankweiler_etl::blob_cas::BlobBundle) the provider
/// pre-loaded from its raw store + sibling CAS in `parse`. Each
/// rendered page calls `bundle.materialize_to_dir(<page_dir>/blobs)` so
/// the markdown's `![](blobs/…)` links resolve. Chats without an entry
/// (or with an empty bundle) render with "(not yet fetched)"
/// placeholders for any attachment that has a `ref_id`.
#[allow(clippy::too_many_arguments)]
pub fn render_all(
    profile: &RenderProfile,
    chats: &[NormalizedChat],
    out_dir: &Path,
    source_name: &str,
    blobs_by_chat: &HashMap<String, BlobBundle>,
    progress: &Progress,
    prior_fingerprints: &HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<RenderSummary> {
    let mut summary = RenderSummary {
        docs_total: chats.iter().map(|c| c.buckets.len()).sum(),
        ..Default::default()
    };
    progress.set_length(Some(summary.docs_total as u64));

    let empty_bundle = BlobBundle::default();
    for chat in chats {
        let bundle = blobs_by_chat.get(&chat.id).unwrap_or(&empty_bundle);
        for doc in &chat.buckets {
            let outcome = render_one(
                profile,
                chat,
                doc,
                out_dir,
                source_name,
                bundle,
                prior_fingerprints,
                on_doc_complete,
            )?;
            match outcome {
                Outcome::Rendered { items, reactions } => {
                    summary.docs_rendered += 1;
                    summary.items_rendered += items;
                    summary.reactions_rendered += reactions;
                }
                Outcome::Skipped => summary.docs_skipped += 1,
            }
            progress.inc(1);
        }
    }
    Ok(summary)
}

enum Outcome {
    Rendered { items: usize, reactions: usize },
    Skipped,
}

#[allow(clippy::too_many_arguments)]
fn render_one(
    profile: &RenderProfile,
    chat: &NormalizedChat,
    doc: &NormalizedDoc,
    out_dir: &Path,
    source_name: &str,
    blobs: &BlobBundle,
    prior_fingerprints: &HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<Outcome> {
    let fingerprint = compute_fingerprint(profile.render_version, chat, doc);
    let (md_path, json_path, page_dir) = output_paths(
        out_dir,
        profile.provider,
        source_name,
        chat,
        &doc.period_key,
    );

    if prior_fingerprints
        .get(&doc.markdown_uuid)
        .map(String::as_str)
        == Some(fingerprint.as_str())
        && md_path.exists()
    {
        return Ok(Outcome::Skipped);
    }
    fs::create_dir_all(&page_dir).with_context(|| format!("mkdir -p {}", page_dir.display()))?;

    // Materialize attachment bytes from blob_cas into <page_dir>/blobs/
    // and rewrite each attachment's `rel_path` to point at the file we
    // just wrote. Mutates a local copy of `doc` — the same chat may be
    // re-rendered into another bucket later, and each bucket needs its
    // own per-page materialization pass.
    let resolved_doc = materialize_attachment_bytes(doc, &page_dir, blobs);
    let doc = &resolved_doc;

    let chat_title = format!(
        "{label} · {disp}",
        label = profile.source_label,
        disp = chat.display
    );
    let doc_title = format!("{chat_title} ({})", doc.period_key);

    let md = render_markdown(profile, chat, doc, &doc_title, &fingerprint);
    fs::write(&md_path, &md).with_context(|| format!("write {}", md_path.display()))?;

    let md_rel = md_path
        .strip_prefix(out_dir)
        .unwrap_or(&md_path)
        .to_string_lossy()
        .into_owned();

    let rows = build_grid_rows(profile, chat, doc, &chat_title, &md_rel);

    emit_sidecar(
        &json_path,
        &doc.markdown_uuid,
        &fingerprint,
        profile.render_version,
        &rows,
        &[],
    )?;

    let items_rendered = doc
        .items
        .iter()
        .filter(|i| !matches!(i.kind, ItemKind::System) || i.text.is_some())
        .count();
    let reactions_rendered = doc.items.iter().map(|i| i.reactions.len()).sum();

    on_doc_complete(RenderedMarkdown {
        markdown_uuid: doc.markdown_uuid.clone(),
        source_name: source_name.to_string(),
        source_fingerprint: fingerprint,
        upstream_cursor: None,
        md_path,
        render_version: profile.render_version,
        rows,
        edges: Vec::new(),
    })
    .with_context(|| format!("on_doc_complete {}", doc.markdown_uuid))?;

    Ok(Outcome::Rendered {
        items: items_rendered,
        reactions: reactions_rendered,
    })
}

/// Write every blob in the per-chat bundle into
/// `<page_dir>/blobs/<short-blake3>.<ext>`, then walk `doc.items` and
/// — for every attachment whose `ref_id` resolves in the bundle — set
/// `rel_path = "blobs/<rendered_filename>"` so the markdown emitter
/// picks up the materialized blob instead of the "(not yet fetched)"
/// placeholder. Same shape slack's bucket-side render uses.
///
/// On any io error from `materialize_to_dir` we log WARN and leave
/// `rel_path` alone — the existing renderer branch already handles
/// the placeholder rendering, and a partial render is strictly better
/// than a hard fail mid-translate.
fn materialize_attachment_bytes(
    doc: &NormalizedDoc,
    page_dir: &Path,
    blobs: &BlobBundle,
) -> NormalizedDoc {
    let mut out = doc.clone();
    if blobs.is_empty() {
        return out;
    }
    if let Err(e) = blobs.materialize_to_dir(&page_dir.join("blobs")) {
        tracing::warn!(
            page_dir = %page_dir.display(),
            error = %e,
            "chat_common::materialize: BlobBundle::materialize_to_dir failed; leaving rel_paths unset"
        );
        return out;
    }
    for item in &mut out.items {
        for att in &mut item.attachments {
            let Some(ref_id) = att.ref_id.as_deref() else {
                continue;
            };
            if let Some(blob) = blobs.get(ref_id) {
                att.rel_path = Some(format!("blobs/{}", blob.rendered_filename()));
            }
        }
    }
    out
}

/// `<out>/rendered_md/<provider>/<source_name>/<chat-slug>/<period>.md`
/// plus the matching sidecar and parent dir. Mirror of slack /
/// beeper's path shape so every provider lands under
/// `rendered_md/<provider>/`.
fn output_paths(
    out_dir: &Path,
    provider: &str,
    source_name: &str,
    chat: &NormalizedChat,
    period_key: &str,
) -> (PathBuf, PathBuf, PathBuf) {
    let chat_slug = format!(
        "chat-{id}__{slug}__{short}",
        id = chat.id,
        slug = slugify(&chat.display),
        short = &chat.chat_uuid[..8.min(chat.chat_uuid.len())],
    );
    let page_dir = out_dir
        .join("rendered_md")
        .join(provider)
        .join(source_name)
        .join(&chat_slug);
    let md_path = page_dir.join(format!("{period_key}.md"));
    let json_path = page_dir.join(format!("{period_key}.grid_rows.json"));
    (md_path, json_path, page_dir)
}

// ─────────────────────────────────────────────────────────────────────
// Markdown
// ─────────────────────────────────────────────────────────────────────

fn render_markdown(
    profile: &RenderProfile,
    chat: &NormalizedChat,
    doc: &NormalizedDoc,
    title: &str,
    fingerprint: &str,
) -> String {
    let mut s = String::with_capacity(8 * 1024);
    s.push_str("---\n");
    s.push_str(&format!("title: \"{}\"\n", title.replace('"', "\\\"")));
    s.push_str(&format!("provider: {}\n", profile.provider));
    s.push_str(&format!("source_label: \"{}\"\n", profile.source_label));
    s.push_str(&format!("chat_uuid: {}\n", chat.chat_uuid));
    s.push_str(&format!("markdown_uuid: {}\n", doc.markdown_uuid));
    s.push_str(&format!("period: {}\n", doc.period_key));
    s.push_str(&format!(
        "display: \"{}\"\n",
        chat.display.replace('"', "\\\"")
    ));
    if let Some(a) = &chat.account {
        s.push_str(&format!("account: {a}\n"));
    }
    if let Some(p) = &chat.project {
        s.push_str(&format!("project: {p}\n"));
    }
    if let Some(e) = &chat.external_id {
        s.push_str(&format!("external_id: {e}\n"));
    }
    s.push_str(&format!("item_count: {}\n", doc.items.len()));
    s.push_str(&format!("source_fingerprint: {fingerprint}\n"));
    s.push_str("---\n\n");

    s.push_str(
        &Title {
            text: title,
            markdown_uuid: Some(&doc.markdown_uuid),
            // Public per-chat URL when the provider has one (LinkedIn
            // post, Slack permalink, …); None for backup-based providers.
            source_url: chat.source_url.as_deref(),
        }
        .render(),
    );

    if doc.items.is_empty() {
        s.push_str("_(no messages)_\n");
        return s;
    }

    for item in &doc.items {
        render_item(&mut s, profile, item);
    }
    s
}

fn render_item(s: &mut String, profile: &RenderProfile, item: &NormalizedChatItem) {
    s.push_str(&msg_div_open(&item.message_uuid, profile.provider));
    s.push_str("\n\n");

    match item.kind {
        ItemKind::System => {
            // Italic small text — keeps system events visible without
            // dominating the transcript. Hidden from grid_row text
            // content too (see build_grid_rows).
            let summary = item
                .system_note
                .as_deref()
                .or(item.text.as_deref())
                .unwrap_or("(system event)");
            s.push_str(&format!(
                "*<small>{ts} — system: {summary}</small>*\n\n",
                ts = display_ts(item.date_ms)
            ));
            s.push_str("</div>\n\n");
            return;
        }
        ItemKind::Text | ItemKind::Attachment => {
            s.push_str("## ");
            s.push_str(&display_ts(item.date_ms));
            s.push_str(" — ");
            s.push_str(&item.author_display);
            s.push('\n');
        }
    }

    match item.kind {
        ItemKind::Text => {
            if let Some(text) = item.text.as_deref().filter(|t| !t.is_empty()) {
                s.push('\n');
                s.push_str(text);
                s.push('\n');
            }
        }
        ItemKind::Attachment => {
            if let Some(caption) = item.text.as_deref().filter(|t| !t.is_empty()) {
                s.push('\n');
                s.push_str(caption);
                s.push('\n');
            }
            if item.attachments.is_empty() {
                s.push_str("\n*[attachment metadata missing]*\n");
            }
            for att in &item.attachments {
                render_attachment(s, att);
            }
        }
        ItemKind::System => unreachable!(),
    }

    if !item.reactions.is_empty() {
        s.push('\n');
        let mut sorted = item.reactions.clone();
        sorted.sort_by(|a, b| {
            a.emoji
                .cmp(&b.emoji)
                .then(a.reactor_display.cmp(&b.reactor_display))
        });
        for r in &sorted {
            // Each reaction gets a `data-section-uuid="<reaction_uuid>"`
            // span so its grid_row's row-click highlights this bullet,
            // matching the per-message anchor convention.
            s.push_str(&format!(
                "- <span id=\"m-{uuid}\" data-section-uuid=\"{uuid}\">{emoji} {who}</span>\n",
                uuid = r.reaction_uuid,
                emoji = r.emoji,
                who = r.reactor_display,
            ));
        }
    }

    s.push_str("\n</div>\n\n");
}

fn render_attachment(s: &mut String, att: &crate::types::NormalizedAttachment) {
    let label = att
        .file_name
        .clone()
        .or_else(|| {
            att.rel_path
                .as_deref()
                .and_then(|p| p.rsplit('/').next())
                .map(str::to_string)
        })
        .unwrap_or_else(|| "attachment".to_string());
    let size = att
        .byte_len
        .map(human_bytes)
        .unwrap_or_else(|| "size unknown".to_string());
    let kind_marker = if att.is_image() {
        "🖼"
    } else if att
        .mime_type
        .as_deref()
        .is_some_and(|m| m.starts_with("video/"))
    {
        "🎞"
    } else if att
        .mime_type
        .as_deref()
        .is_some_and(|m| m.starts_with("audio/"))
    {
        "🔊"
    } else {
        "📎"
    };

    let is_audio = att
        .mime_type
        .as_deref()
        .is_some_and(|m| m.starts_with("audio/"));
    let is_video = att
        .mime_type
        .as_deref()
        .is_some_and(|m| m.starts_with("video/"));

    s.push('\n');
    match &att.rel_path {
        Some(rel) if att.is_image() => {
            s.push_str(&format!("![{label}]({rel})\n"));
        }
        // Inline HTML5 players so audio/video attachments play straight
        // from the markdown viewer (which already passes raw HTML through
        // — see the `<div class="msg">` wrappers). The labelled link
        // underneath is a fallback for renderers that strip media tags.
        Some(rel) if is_audio => {
            s.push_str(&format!(
                "<audio controls src=\"{rel}\"></audio>\n\n{kind_marker} [{label}]({rel}) — {size}\n"
            ));
        }
        Some(rel) if is_video => {
            s.push_str(&format!(
                "<video controls src=\"{rel}\"></video>\n\n{kind_marker} [{label}]({rel}) — {size}\n"
            ));
        }
        Some(rel) => {
            s.push_str(&format!("{kind_marker} [{label}]({rel}) — {size}\n"));
        }
        None => {
            s.push_str(&format!("{kind_marker} *[{label} (not yet fetched)]*\n",));
            if let Some(url) = &att.source_url {
                s.push_str(&format!("*(source: {url})*\n"));
            }
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Grid rows
// ─────────────────────────────────────────────────────────────────────

fn build_grid_rows(
    profile: &RenderProfile,
    chat: &NormalizedChat,
    doc: &NormalizedDoc,
    chat_title: &str,
    md_rel: &str,
) -> Vec<GridRow> {
    let mut rows: Vec<GridRow> = Vec::with_capacity(1 + doc.items.len());

    let first_ts = doc
        .items
        .first()
        .map(|i| iso_from_ms(i.date_ms))
        .unwrap_or_else(|| iso_from_ms(0));
    let conversation_name = Some(chat.display.clone());
    let entire_chat = format!("/chat/{}", doc.markdown_uuid);

    rows.push(GridRow {
        uuid: doc.markdown_uuid.clone(),
        provider: profile.provider.to_string(),
        kind: profile.chat_kind.clone(),
        source_label: profile.source_label.clone(),
        when_ts: Some(first_ts),
        author: None,
        account: chat.account.clone(),
        org_uuid: None,
        org_name: None,
        project: chat.project.clone(),
        channel: conversation_name.clone(),
        conversation_name: conversation_name.clone(),
        conversation_uuid: chat.chat_uuid.clone(),
        message_index: None,
        entire_chat: entire_chat.clone(),
        text: doc
            .items
            .iter()
            .filter(|i| !matches!(i.kind, ItemKind::System))
            .filter_map(|i| i.text.clone())
            .collect::<Vec<_>>()
            .join("\n"),
        slack_link: None,
        qmd_path: Some(md_rel.to_string()),
        source_url: chat.source_url.clone(),
        git_sha: None,
        external_id: chat.external_id.clone(),
        notion_page_uuid: None,
        notion_block_uuid: None,
        markdown_uuid: Some(doc.markdown_uuid.clone()),
    });

    let _ = chat_title; // reserved for future per-message title context

    for (idx, item) in doc.items.iter().enumerate() {
        let text = match item.kind {
            ItemKind::Text => item.text.clone().unwrap_or_default(),
            ItemKind::Attachment => item
                .text
                .clone()
                .unwrap_or_else(|| attachment_search_text(item)),
            ItemKind::System => item
                .system_note
                .clone()
                .or_else(|| item.text.clone())
                .unwrap_or_default(),
        };
        rows.push(GridRow {
            uuid: item.message_uuid.clone(),
            provider: profile.provider.to_string(),
            kind: profile.message_kind.clone(),
            source_label: profile.source_label.clone(),
            when_ts: Some(iso_from_ms(item.date_ms)),
            author: Some(item.author_display.clone()),
            account: chat.account.clone(),
            org_uuid: None,
            org_name: None,
            project: chat.project.clone(),
            channel: conversation_name.clone(),
            conversation_name: conversation_name.clone(),
            conversation_uuid: chat.chat_uuid.clone(),
            message_index: Some(idx as i64),
            entire_chat: entire_chat.clone(),
            text,
            slack_link: None,
            qmd_path: Some(md_rel.to_string()),
            source_url: item.attachments.iter().find_map(|a| a.source_url.clone()),
            git_sha: None,
            external_id: None,
            notion_page_uuid: None,
            notion_block_uuid: None,
            markdown_uuid: Some(doc.markdown_uuid.clone()),
        });
        for r in &item.reactions {
            rows.push(GridRow {
                uuid: r.reaction_uuid.clone(),
                provider: profile.provider.to_string(),
                kind: profile.reaction_kind.clone(),
                source_label: profile.source_label.clone(),
                when_ts: Some(iso_from_ms(r.date_ms)),
                author: Some(r.reactor_display.clone()),
                account: chat.account.clone(),
                org_uuid: None,
                org_name: None,
                project: chat.project.clone(),
                channel: conversation_name.clone(),
                conversation_name: conversation_name.clone(),
                conversation_uuid: chat.chat_uuid.clone(),
                message_index: None,
                entire_chat: entire_chat.clone(),
                text: r.emoji.clone(),
                slack_link: None,
                qmd_path: Some(md_rel.to_string()),
                source_url: None,
                git_sha: None,
                external_id: None,
                notion_page_uuid: None,
                notion_block_uuid: None,
                markdown_uuid: Some(doc.markdown_uuid.clone()),
            });
        }
    }
    rows
}

fn attachment_search_text(item: &NormalizedChatItem) -> String {
    item.attachments
        .iter()
        .filter_map(|a| a.file_name.clone())
        .collect::<Vec<_>>()
        .join(" ")
}

// ─────────────────────────────────────────────────────────────────────
// Fingerprint
// ─────────────────────────────────────────────────────────────────────

fn compute_fingerprint(render_version: u32, chat: &NormalizedChat, doc: &NormalizedDoc) -> String {
    let mut h = Sha256::new();
    h.update(render_version.to_be_bytes());
    h.update(b"|");
    h.update(chat.chat_uuid.as_bytes());
    h.update(b"|");
    h.update(doc.period_key.as_bytes());
    // Fold the chat-level linkout in only when present, so providers that
    // don't set one keep their existing fingerprints (no forced
    // re-render); a changed/added URL re-renders the `↗` in the title.
    if let Some(url) = &chat.source_url {
        h.update(b"|src|");
        h.update(url.as_bytes());
    }
    for item in &doc.items {
        h.update(b"\n");
        h.update(item.message_uuid.as_bytes());
        h.update(b"|");
        h.update(item.author_id.as_bytes());
        h.update(b"|");
        h.update(item.date_ms.to_be_bytes());
        h.update(b"|");
        h.update(item.text.as_deref().unwrap_or("").as_bytes());
        h.update(b"|");
        h.update((item.attachments.len() as u32).to_be_bytes());
        for a in &item.attachments {
            // Hash `ref_id` (the source-of-truth pointer into blob_cas)
            // rather than `rel_path` — `rel_path` is filled in at
            // render time, so hashing it would defeat the
            // "compute the fingerprint up front" pattern. file_name
            // mixed in so a renamed but otherwise identical attachment
            // still triggers a re-render.
            h.update(a.ref_id.as_deref().unwrap_or("").as_bytes());
            h.update(b"+");
            h.update(a.file_name.as_deref().unwrap_or("").as_bytes());
        }
        h.update(b"|");
        h.update((item.reactions.len() as u32).to_be_bytes());
        let mut reacts = item.reactions.clone();
        reacts.sort_by(|a, b| a.reaction_uuid.cmp(&b.reaction_uuid));
        for r in &reacts {
            h.update(r.reaction_uuid.as_bytes());
            h.update(b"+");
            h.update(r.emoji.as_bytes());
        }
    }
    format!("{:x}", h.finalize())
}

// ─────────────────────────────────────────────────────────────────────
// Format helpers
// ─────────────────────────────────────────────────────────────────────

fn iso_from_ms(ms: i64) -> String {
    IsoOffsetTimestamp::from_unix_millis(ms)
        .map(|t| t.to_rfc3339_secs())
        .unwrap_or_else(|| {
            tracing::warn!(ms, "iso_from_ms: epoch-ms out of chrono range");
            "1970-01-01T00:00:00+00:00".to_string()
        })
}

fn display_ts(ms: i64) -> String {
    IsoOffsetTimestamp::from_unix_millis(ms)
        .map(|t| t.inner().format("%Y-%m-%d %H:%M:%S UTC").to_string())
        .unwrap_or_else(|| format!("@{ms}ms"))
}

fn human_bytes(n: i64) -> String {
    let n = n as f64;
    if n < 1024.0 {
        format!("{} B", n as i64)
    } else if n < 1024.0 * 1024.0 {
        format!("{:.1} KiB", n / 1024.0)
    } else if n < 1024.0 * 1024.0 * 1024.0 {
        format!("{:.1} MiB", n / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GiB", n / (1024.0 * 1024.0 * 1024.0))
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{NormalizedAttachment, NormalizedReaction};

    fn mk_chat() -> NormalizedChat {
        NormalizedChat {
            id: "100".to_string(),
            chat_uuid: "11111111-1111-1111-1111-111111111111".to_string(),
            display: "Bridge Crew".to_string(),
            account: Some("acct-1".to_string()),
            project: None,
            external_id: Some("bridge-crew@g.us".to_string()),
            source_url: None,
            buckets: vec![NormalizedDoc {
                period_key: "2364-04".to_string(),
                markdown_uuid: "22222222-2222-2222-2222-222222222222".to_string(),
                items: vec![NormalizedChatItem {
                    message_uuid: "33333333-3333-3333-3333-333333333333".to_string(),
                    author_id: "1".to_string(),
                    author_display: "Picard".to_string(),
                    date_ms: 12442118400000,
                    text: Some("Make it so.".to_string()),
                    kind: ItemKind::Text,
                    attachments: vec![],
                    reactions: vec![NormalizedReaction {
                        reaction_uuid: "44444444-4444-4444-4444-444444444444".to_string(),
                        reactor_display: "Will Riker".to_string(),
                        emoji: "🫡".to_string(),
                        date_ms: 12442118410000,
                    }],
                    system_note: None,
                }],
            }],
        }
    }

    #[test]
    fn fingerprint_is_stable_across_runs() {
        let chat = mk_chat();
        let fp1 = compute_fingerprint(1, &chat, &chat.buckets[0]);
        let fp2 = compute_fingerprint(1, &chat, &chat.buckets[0]);
        assert_eq!(fp1, fp2);
    }

    #[test]
    fn fingerprint_changes_with_render_version() {
        let chat = mk_chat();
        let fp1 = compute_fingerprint(1, &chat, &chat.buckets[0]);
        let fp2 = compute_fingerprint(2, &chat, &chat.buckets[0]);
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn fingerprint_changes_with_reaction() {
        let chat1 = mk_chat();
        let mut chat2 = mk_chat();
        chat2.buckets[0].items[0].reactions[0].emoji = "👍".to_string();
        let fp1 = compute_fingerprint(1, &chat1, &chat1.buckets[0]);
        let fp2 = compute_fingerprint(1, &chat2, &chat2.buckets[0]);
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn renders_basic_text_item_with_reaction() {
        let profile = RenderProfile {
            provider: "test",
            source_label: "Test".to_string(),
            chat_kind: "Test Chat".to_string(),
            message_kind: "Test Message".to_string(),
            reaction_kind: "Test Reaction".to_string(),
            render_version: 1,
        };
        let chat = mk_chat();
        let md = render_markdown(
            &profile,
            &chat,
            &chat.buckets[0],
            "Test · Bridge Crew",
            "fp",
        );
        assert!(md.contains("Make it so."));
        assert!(md.contains("🫡 Will Riker"));
        assert!(md.contains("id=\"m-33333333"));
        assert!(md.contains("id=\"m-44444444"));
    }

    #[test]
    fn attachment_without_rel_path_falls_back_to_placeholder() {
        let mut chat = mk_chat();
        chat.buckets[0].items[0] = NormalizedChatItem {
            kind: ItemKind::Attachment,
            text: Some("Viewscreen capture".to_string()),
            attachments: vec![NormalizedAttachment {
                rel_path: None,
                file_name: Some("bridge-viewscreen.jpg".to_string()),
                mime_type: Some("image/jpeg".to_string()),
                byte_len: Some(384),
                source_url: Some("https://example/vscapture".to_string()),
                ref_id: None,
            }],
            ..chat.buckets[0].items[0].clone()
        };
        let profile = RenderProfile {
            provider: "test",
            source_label: "Test".to_string(),
            chat_kind: "Test Chat".to_string(),
            message_kind: "Test Message".to_string(),
            reaction_kind: "Test Reaction".to_string(),
            render_version: 1,
        };
        let md = render_markdown(&profile, &chat, &chat.buckets[0], "Test", "fp");
        assert!(md.contains("not yet fetched"));
        assert!(md.contains("https://example/vscapture"));
    }

    #[test]
    fn chat_source_url_surfaces_in_title_and_chat_grid_row() {
        let profile = RenderProfile {
            provider: "test",
            source_label: "Test".to_string(),
            chat_kind: "Test Chat".to_string(),
            message_kind: "Test Message".to_string(),
            reaction_kind: "Test Reaction".to_string(),
            render_version: 1,
        };
        let mut chat = mk_chat();
        chat.source_url = Some("https://example.com/post/42".to_string());

        // Title gets the `↗` source link.
        let md = render_markdown(&profile, &chat, &chat.buckets[0], "Test", "fp");
        assert!(
            md.contains("class=\"source-link\"") && md.contains("https://example.com/post/42"),
            "title carries the source linkout: {md}"
        );

        // The chat-level grid row (first row) carries it too.
        let rows = build_grid_rows(&profile, &chat, &chat.buckets[0], "Test", "x.md");
        assert_eq!(rows[0].kind, profile.chat_kind);
        assert_eq!(
            rows[0].source_url.as_deref(),
            Some("https://example.com/post/42")
        );
    }

    #[test]
    fn fingerprint_tracks_source_url() {
        // None (the default) keeps the pre-existing fingerprint stable…
        let none = mk_chat();
        let mut bare = mk_chat();
        bare.source_url = None;
        assert_eq!(
            compute_fingerprint(1, &none, &none.buckets[0]),
            compute_fingerprint(1, &bare, &bare.buckets[0]),
        );
        // …while setting / changing it re-cuts the fingerprint.
        let mut set = mk_chat();
        set.source_url = Some("https://example.com/a".to_string());
        assert_ne!(
            compute_fingerprint(1, &none, &none.buckets[0]),
            compute_fingerprint(1, &set, &set.buckets[0]),
        );
    }
}
