//! `render_all` — drives every `(chat, period)` bucket through
//! [`render_one`], handles fingerprint-skip, and feeds rendered docs
//! into the orchestrator's `on_doc_complete` callback.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Default `upstream_entity_kind` for a chat/thread-level row — the
/// value most providers put in [`RenderProfile::chat_entity_kind`].
pub const ENTITY_KIND_CONVERSATION: &str = "conversation";

/// The markdown layout in this file has its own version, and it is
/// hashed into every document's fingerprint beside the provider's
/// [`RenderProfile::render_version`].
///
/// **Bump this whenever `render_markdown` changes what it writes.**
/// Without it, changing the shared layout means editing the version
/// constant in all eight providers by hand and re-rendering nothing
/// in the one you forget. The provider's own number stays its own —
/// `datalib_step`'s render step checks that every version stored on
/// disk is one its processors declare, so this must not be mixed into
/// the stored value.
pub const LAYOUT_VERSION: u32 = 3;

use anyhow::{Context, Result};
use datalib_etl::blob_cas::BlobBundle;
use datalib_etl::progress::Progress;
use datalib_etl::title::Title;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::section::msg_div_open;
use datalib_schema::grid_rows::GridRow;
use datalib_schema::providers::Provider;
use datalib_schema::render_problems::RenderProblemRow;
use sha2::{Digest, Sha256};

use crate::html::{escape_attr, escape_text};
use crate::types::{ItemKind, NormalizedChat, NormalizedChatItem, NormalizedDoc};

/// Per-provider knobs the renderer parameterizes on. Values that
/// would otherwise be hard-coded as `"signal"` / `"Signal Chat"` /
/// `"Signal Message"` so a single render function serves every chat
/// provider.
#[derive(Debug, Clone)]
pub struct RenderProfile {
    /// On-disk subdir under `rendered_md/<provider>/<source_name>/…`
    /// and the value of the markdown's `provider:` frontmatter key.
    pub provider: Provider,
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
    /// `grid_rows.upstream_entity_kind` for this profile's chat-level
    /// rows — the `entity_kind` component of the `datalib_id` recipe
    /// that minted their `uuid`.
    pub chat_entity_kind: &'static str,
    /// Each provider bumps its own render version when its render
    /// layer changes meaningfully (column changes, item-shape changes,
    /// new field on grid_rows). The chat-common renderer stamps this
    /// into the store so a re-run knows to invalidate stale docs.
    pub render_version: u32,
}

#[derive(Debug, Default, Clone)]
pub struct RenderSummary {
    pub docs_total: usize,
    pub docs_rendered: usize,
    pub docs_skipped: usize,
    pub items_rendered: usize,
    pub reactions_rendered: usize,
    /// Every document this call *considered*, rendered and skipped alike.
    ///
    /// Skipped ones belong here and that is the whole point: a caller uses
    /// this to tell "still there, unchanged" from "gone", and one that saw
    /// only re-rendered documents would read its own steady state as a
    /// mass deletion. Meaningful only to a caller that handed over every
    /// chat its store holds — see `RunCtx::retain_documents`.
    pub documents: Vec<String>,
}

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
            summary.documents.push(doc.markdown_uuid.clone());
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
    let fingerprint = compute_fingerprint(profile.render_version, LAYOUT_VERSION, chat, doc);
    let (md_path, page_dir) = output_paths(out_dir, source_name, chat, &doc.period_key);

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

    // A provider-supplied `title` takes the `<h1>`; otherwise derive the
    // familiar "{source_label} · {display}" heading.
    let chat_title = match &chat.title {
        Some(t) => t.clone(),
        None => format!(
            "{label} · {disp}",
            label = profile.source_label,
            disp = chat.display
        ),
    };
    let doc_title = format!("{chat_title} ({})", doc.period_key);

    let md = render_markdown(profile, chat, doc, &doc_title, &fingerprint);
    fs::write(&md_path, &md).with_context(|| format!("write {}", md_path.display()))?;

    let md_rel = md_path
        .strip_prefix(out_dir)
        .unwrap_or(&md_path)
        .to_string_lossy()
        .into_owned();

    let mut problems: Vec<RenderProblemRow> = Vec::new();
    let rows = build_grid_rows(
        profile,
        chat,
        doc,
        &chat_title,
        &md_rel,
        source_name,
        &mut problems,
    );

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
        problems,
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
/// `rel_path = "blobs/<filename_for(ref)>"` so the markdown emitter
/// picks up the materialized blob instead of the "(not yet fetched)"
/// placeholder. Same shape slack's bucket-side render uses.
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
            if let Some(fname) = blobs.filename_for(ref_id) {
                att.rel_path = Some(format!("blobs/{fname}"));
            }
        }
    }
    out
}

/// `<out>/<stanza>/rendered_md/<chat_uuid>/<period>.md` plus the matching
/// markdown and its parent dir. The directory is the chat's stable UUID — never a
/// title-derived slug — so an upstream rename (channel/title change)
/// re-renders in place instead of orphaning the old file at a stale path. The
/// human title still lives in the markdown frontmatter and the grid_rows DB.
fn output_paths(
    out_dir: &Path,
    source_name: &str,
    chat: &NormalizedChat,
    period_key: &str,
) -> (PathBuf, PathBuf) {
    let page_dir =
        datalib_etl::layout::rendered_md_root(out_dir, source_name).join(&chat.chat_uuid);
    let md_path = page_dir.join(format!("{period_key}.md"));
    (md_path, page_dir)
}

// Markdown

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

    let mut i = 0;
    while i < doc.items.len() {
        let run_end = doc.items[i..]
            .iter()
            .position(|it| !it.is_aside)
            .map_or(doc.items.len(), |n| i + n);
        if run_end > i {
            render_aside_run(&mut s, profile, &doc.items[i..run_end]);
            i = run_end;
        } else {
            render_item(&mut s, profile, &doc.items[i]);
            i += 1;
        }
    }
    s
}

/// Wrap one run of adjacent asides in a single collapsed `<details>`.
///
/// The `<details>` sits *outside* the per-message `<div>`s so every
/// anchor, copy button and grid-row highlight inside it keeps working
/// unchanged — the frontend opens the enclosing `<details>` when it
/// scrolls to a section within one.
fn render_aside_run(s: &mut String, profile: &RenderProfile, items: &[NormalizedChatItem]) {
    let plural = if items.len() == 1 { "" } else { "s" };
    s.push_str(&format!(
        "<details class=\"tool-group\">\n<summary>🛠 {n} tool step{plural}</summary>\n\n",
        n = items.len(),
    ));
    for item in items {
        render_item(s, profile, item);
    }
    s.push_str("</details>\n\n");
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
                ts = short_ts(item.date_ms)
            ));
            s.push_str("</div>\n\n");
            return;
        }
        ItemKind::Text | ItemKind::Attachment => {
            // A real `##` heading, whose parts are tagged so the UI can
            // style it down to a Slack-style one-liner. The heading is
            // load-bearing beyond looks: qmd cuts its chunks at the
            // best nearby break point and scores an `h2` far above the
            // blank line it would otherwise settle for, so every
            // message start is also a chunk boundary it prefers.
            s.push_str("## ");
            s.push_str(&format!(
                "<span class=\"msg-author\">{}</span> ",
                escape_text(&item.author_display),
            ));
            s.push_str(&timestamp_html(item.date_ms));
            // Per-message linkout (e.g. a Slack permalink) as a `↗` after
            // the header, opening in a new tab.
            if let Some(url) = &item.source_url {
                s.push_str(&format!(
                    " <a class=\"source-link\" href=\"{url}\" target=\"_blank\" rel=\"noopener noreferrer\">↗</a>",
                    url = escape_attr(url),
                ));
            }
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

// Grid rows

#[allow(clippy::too_many_arguments)]
fn build_grid_rows(
    profile: &RenderProfile,
    chat: &NormalizedChat,
    doc: &NormalizedDoc,
    chat_title: &str,
    md_rel: &str,
    source_name: &str,
    problems: &mut Vec<RenderProblemRow>,
) -> Vec<GridRow> {
    let mut rows: Vec<GridRow> = Vec::with_capacity(1 + doc.items.len());

    // The chat-level row is stamped with the earliest *real* timestamp
    // in the bucket. `min()` over the items that have one rather than
    // "the first item's", for two reasons: an undated item sorts to the
    // front (`None < Some` in every provider's `sort_by_key`), so
    // reading item 0 would hand a whole conversation a null; and a
    // bucket with no dated item at all — including the empty bucket
    // `render_markdown` renders as "_(no messages)_" — then correctly
    // gets `None` instead of the 1970 stamp it used to get. Providers
    // sort ascending, so for a fully-dated bucket this is the same value
    // `items.first()` gave.
    let first_ts = when_ts_from_ms(doc.items.iter().filter_map(|i| i.date_ms).min());
    let conversation_name = Some(chat.display.clone());
    let entire_chat = format!("/chat/{}", doc.markdown_uuid);

    rows.extend(
        GridRow::builder()
            .uuid(doc.markdown_uuid.clone())
            .provider(profile.provider)
            .kind(profile.chat_kind.clone())
            .source_label(profile.source_label.clone())
            .when_ts(first_ts)
            .account(chat.account.clone())
            .org_uuid(chat.org_uuid.clone())
            .org_name(chat.org_name.clone())
            .project(chat.project.clone())
            .channel(conversation_name.clone())
            .conversation_name(conversation_name.clone())
            .conversation_uuid(chat.chat_uuid.clone())
            .entire_chat(entire_chat.clone())
            .text(
                doc.items
                    .iter()
                    .filter(|i| !matches!(i.kind, ItemKind::System))
                    .filter_map(|i| i.text.clone())
                    .collect::<Vec<_>>()
                    .join("\n"),
            )
            .qmd_path(Some(md_rel.to_string()))
            .source_url(chat.source_url.clone())
            .upstream_id(chat.external_id.clone())
            .upstream_entity_kind(Some(profile.chat_entity_kind.to_string()))
            .upstream_scope(chat.upstream_scope.clone())
            .markdown_uuid(Some(doc.markdown_uuid.clone()))
            .build_or_record(
                source_name,
                &doc.markdown_uuid,
                profile.render_version,
                problems,
            ),
    );

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
        rows.extend(
            GridRow::builder()
                .uuid(item.message_uuid.clone())
                .provider(profile.provider)
                // Per-item role override (ChatGPT/Anthropic) or the profile default.
                .kind(
                    item.kind_label
                        .clone()
                        .unwrap_or_else(|| profile.message_kind.clone()),
                )
                .source_label(profile.source_label.clone())
                .upstream_id(item.source_ref.as_ref().map(|r| r.native_id.clone()))
                .upstream_entity_kind(item.source_ref.as_ref().map(|r| r.entity_kind.clone()))
                // Items inherit the chat's scope: a chat belongs to
                // exactly one workspace/account, and every row inside
                // it was minted under that same `Scope::Upstream`.
                .upstream_scope(chat.upstream_scope.clone())
                .when_ts(when_ts_from_ms(item.date_ms))
                .author(Some(item.author_display.clone()))
                .account(chat.account.clone())
                .org_uuid(chat.org_uuid.clone())
                .org_name(chat.org_name.clone())
                .project(chat.project.clone())
                .channel(conversation_name.clone())
                .conversation_name(conversation_name.clone())
                .conversation_uuid(chat.chat_uuid.clone())
                .message_index(Some(idx as i64))
                .entire_chat(entire_chat.clone())
                .text(text)
                .qmd_path(Some(md_rel.to_string()))
                // Per-message linkout wins; fall back to an attachment's URL.
                .source_url(
                    item.source_url
                        .clone()
                        .or_else(|| item.attachments.iter().find_map(|a| a.source_url.clone())),
                )
                .markdown_uuid(Some(doc.markdown_uuid.clone()))
                .build_or_record(
                    source_name,
                    &doc.markdown_uuid,
                    profile.render_version,
                    problems,
                ),
        );
        for r in &item.reactions {
            rows.extend(
                GridRow::builder()
                    .uuid(r.reaction_uuid.clone())
                    .provider(profile.provider)
                    .kind(profile.reaction_kind.clone())
                    .source_label(profile.source_label.clone())
                    .upstream_id(r.source_ref.as_ref().map(|s| s.native_id.clone()))
                    .upstream_entity_kind(r.source_ref.as_ref().map(|s| s.entity_kind.clone()))
                    .upstream_scope(chat.upstream_scope.clone())
                    .when_ts(when_ts_from_ms(r.date_ms))
                    .author(Some(r.reactor_display.clone()))
                    .account(chat.account.clone())
                    .org_uuid(chat.org_uuid.clone())
                    .org_name(chat.org_name.clone())
                    .project(chat.project.clone())
                    .channel(conversation_name.clone())
                    .conversation_name(conversation_name.clone())
                    .conversation_uuid(chat.chat_uuid.clone())
                    .entire_chat(entire_chat.clone())
                    .text(r.emoji.clone())
                    .qmd_path(Some(md_rel.to_string()))
                    .markdown_uuid(Some(doc.markdown_uuid.clone()))
                    .build_or_record(
                        source_name,
                        &doc.markdown_uuid,
                        profile.render_version,
                        problems,
                    ),
            );
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

// Fingerprint

/// `layout_version` is always [`LAYOUT_VERSION`] in production; it is a
/// parameter so a test can prove the shared layout really does reach
/// the hash.
fn compute_fingerprint(
    render_version: u32,
    layout_version: u32,
    chat: &NormalizedChat,
    doc: &NormalizedDoc,
) -> String {
    let mut h = Sha256::new();
    h.update(render_version.to_be_bytes());
    h.update(b"|");
    h.update(layout_version.to_be_bytes());
    h.update(b"|");
    h.update(chat.chat_uuid.as_bytes());
    h.update(b"|");
    h.update(doc.period_key.as_bytes());
    // Fold chat-level linkout + title in only when present, so providers
    // that don't set them keep their existing fingerprints (no forced
    // re-render); a change re-renders the `↗` / `<h1>`.
    if let Some(url) = &chat.source_url {
        h.update(b"|src|");
        h.update(url.as_bytes());
    }
    if let Some(title) = &chat.title {
        h.update(b"|title|");
        h.update(title.as_bytes());
    }
    if let Some(org) = &chat.org_uuid {
        h.update(b"|org|");
        h.update(org.as_bytes());
    }
    if let Some(org_name) = &chat.org_name {
        h.update(b"|orgn|");
        h.update(org_name.as_bytes());
    }
    for item in &doc.items {
        h.update(b"\n");
        h.update(item.message_uuid.as_bytes());
        h.update(b"|");
        h.update(item.author_id.as_bytes());
        h.update(b"|");
        // Tag the presence of a timestamp before its bytes, so "no
        // timestamp" and "the epoch" hash differently. Without the tag
        // an item that gains a real `0` stamp — or loses one and falls
        // back to null — would keep its old fingerprint and never
        // re-render.
        match item.date_ms {
            Some(ms) => {
                h.update([1u8]);
                h.update(ms.to_be_bytes());
            }
            None => h.update([0u8]),
        }
        h.update(b"|");
        h.update(item.text.as_deref().unwrap_or("").as_bytes());
        if let Some(url) = &item.source_url {
            h.update(b"|msrc|");
            h.update(url.as_bytes());
        }
        if let Some(k) = &item.kind_label {
            h.update(b"|kind|");
            h.update(k.as_bytes());
        }
        if item.is_aside {
            h.update(b"|aside|");
        }
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
    h.finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>()
}

// Format helpers
// `when_ts_from_ms` / `display_ts` used to live here. Both were the
// timestamp policy rather than anything chat-shaped, and beeper and
// signal each carried their own drifting copy, so they moved to
// `datalib-time` — see the note on `when_ts_from_unix_millis`.
use datalib_time::{
    display_ts_from_unix_millis, when_ts_from_unix_millis, IsoOffsetTimestamp, WhenTsPrecision,
};

/// Seconds precision, as this renderer has always emitted. Changing it
/// would re-cut every fingerprint chat-common has written.
fn when_ts_from_ms(ms: Option<i64>) -> Option<String> {
    when_ts_from_unix_millis(ms, WhenTsPrecision::Seconds)
}

fn display_ts(ms: Option<i64>) -> String {
    display_ts_from_unix_millis(ms)
}

/// Short stamp on screen, full instant on hover.
fn short_ts(ms: Option<i64>) -> String {
    match ms.and_then(IsoOffsetTimestamp::from_unix_millis) {
        Some(t) => datalib_time::short_ts(&t),
        // Either no stamp at all or a number that is not an instant.
        // `display_ts` spells those two apart and neither has a short
        // form worth inventing.
        None => display_ts(ms),
    }
}

/// The message header's timestamp: a `<time>` a reader can hover for
/// the full instant, or a plain span when there is no instant to
/// state.
fn timestamp_html(ms: Option<i64>) -> String {
    match ms.and_then(IsoOffsetTimestamp::from_unix_millis) {
        Some(t) => format!(
            "<time class=\"msg-ts\" datetime=\"{iso}\" title=\"{full}\">{short}</time>",
            iso = t.to_rfc3339_secs(),
            full = display_ts(ms),
            short = datalib_time::short_ts(&t),
        ),
        None => format!("<span class=\"msg-ts\">{}</span>", display_ts(ms)),
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_schema::render_problems::{Problem, Reason};

    fn rows_of(profile: &RenderProfile, chat: &NormalizedChat) -> Vec<GridRow> {
        let mut problems = Vec::new();
        let rows = build_grid_rows(
            profile,
            chat,
            &chat.buckets[0],
            "Test",
            "x.md",
            "test_source",
            &mut problems,
        );
        assert!(problems.is_empty(), "unexpected drops: {problems:?}");
        rows
    }
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
            upstream_scope: None,
            title: None,
            org_uuid: None,
            org_name: None,
            buckets: vec![NormalizedDoc {
                period_key: "2364-04".to_string(),
                markdown_uuid: "22222222-2222-2222-2222-222222222222".to_string(),
                items: vec![NormalizedChatItem {
                    message_uuid: "33333333-3333-3333-3333-333333333333".to_string(),
                    author_id: "1".to_string(),
                    author_display: "Picard".to_string(),
                    date_ms: Some(12442118400000),
                    text: Some("Make it so.".to_string()),
                    kind: ItemKind::Text,
                    attachments: vec![],
                    reactions: vec![NormalizedReaction {
                        reaction_uuid: "44444444-4444-4444-4444-444444444444".to_string(),
                        reactor_display: "Will Riker".to_string(),
                        emoji: "🫡".to_string(),
                        date_ms: Some(12442118410000),
                        source_ref: None,
                    }],
                    system_note: None,
                    source_url: None,
                    kind_label: None,
                    source_ref: None,
                    is_aside: false,
                }],
            }],
        }
    }

    /// One unusable message must cost that message and nothing else.
    #[test]
    fn an_unbuildable_message_is_dropped_and_recorded_not_propagated() {
        let profile = test_profile();
        let mut chat = mk_chat();
        // No `message_uuid` — nothing to key the row on. `build`
        // rejects it as an empty required field.
        chat.buckets[0].items[0].message_uuid = String::new();

        let mut problems = Vec::new();
        let rows = build_grid_rows(
            &profile,
            &chat,
            &chat.buckets[0],
            "Test",
            "x.md",
            "test_source",
            &mut problems,
        );

        // The chat row and the reaction row still made it: a bad
        // message does not take its neighbours with it.
        assert!(
            rows.iter().any(|r| r.kind == profile.chat_kind),
            "the chat-level row survives: {rows:?}"
        );
        assert!(
            rows.iter().any(|r| r.kind == profile.reaction_kind),
            "the reaction row survives"
        );
        assert!(
            !rows.iter().any(|r| r.kind == profile.message_kind),
            "…and the unbuildable message row is not among them"
        );

        assert_eq!(problems.len(), 1, "exactly one problem: {problems:?}");
        let p = &problems[0];
        assert_eq!(p.outcome, "dropped");
        assert_eq!(p.stage, "grid_row");
        assert_eq!(p.source_name, "test_source");
        assert_eq!(
            p.scope_key, chat.buckets[0].markdown_uuid,
            "swept with the document it belongs to"
        );
        assert_eq!(p.scope_kind, "markdown");
        assert_eq!(p.render_version, i64::from(profile.render_version));
        // A row with no uuid gets the content-derived surrogate, so the
        // same bad record does not accumulate a new row every run.
        assert!(p.uuid.starts_with("noid:"), "{}", p.uuid);
        // Never a count without a reason.
        let parsed: Vec<Problem> = serde_json::from_str(&p.problems).expect("problems json");
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].reason, Reason::NoIdentity);
        assert_eq!(parsed[0].field.as_deref(), Some("uuid"));
        // Stamping is the store's job, not the renderer's.
        assert!(p.first_seen_at.is_empty() && p.last_seen_at.is_empty());
    }

    /// The surrogate is content-derived, so a record that stays broken
    /// keeps one row across runs rather than growing one per run.
    #[test]
    fn the_same_bad_record_keys_to_the_same_surrogate_twice() {
        let profile = test_profile();
        let mut chat = mk_chat();
        chat.buckets[0].items[0].message_uuid = String::new();
        let mut a = Vec::new();
        let mut b = Vec::new();
        let args = ("Test", "x.md", "test_source");
        build_grid_rows(
            &profile,
            &chat,
            &chat.buckets[0],
            args.0,
            args.1,
            args.2,
            &mut a,
        );
        build_grid_rows(
            &profile,
            &chat,
            &chat.buckets[0],
            args.0,
            args.1,
            args.2,
            &mut b,
        );
        assert_eq!(a[0].uuid, b[0].uuid);
    }

    #[test]
    fn fingerprint_is_stable_across_runs() {
        let chat = mk_chat();
        let fp1 = compute_fingerprint(1, LAYOUT_VERSION, &chat, &chat.buckets[0]);
        let fp2 = compute_fingerprint(1, LAYOUT_VERSION, &chat, &chat.buckets[0]);
        assert_eq!(fp1, fp2);
    }

    #[test]
    fn fingerprint_changes_with_render_version() {
        let chat = mk_chat();
        let fp1 = compute_fingerprint(1, LAYOUT_VERSION, &chat, &chat.buckets[0]);
        let fp2 = compute_fingerprint(2, LAYOUT_VERSION, &chat, &chat.buckets[0]);
        assert_ne!(fp1, fp2);
    }

    /// Bumping the shared layout must re-render every chat provider
    /// without any of them touching its own `RENDER_VERSION` — that is
    /// the whole reason [`LAYOUT_VERSION`] exists.
    #[test]
    fn fingerprint_changes_with_layout_version() {
        let chat = mk_chat();
        assert_ne!(
            compute_fingerprint(1, LAYOUT_VERSION, &chat, &chat.buckets[0]),
            compute_fingerprint(1, LAYOUT_VERSION + 1, &chat, &chat.buckets[0]),
        );
    }

    #[test]
    fn fingerprint_changes_with_reaction() {
        let chat1 = mk_chat();
        let mut chat2 = mk_chat();
        chat2.buckets[0].items[0].reactions[0].emoji = "👍".to_string();
        let fp1 = compute_fingerprint(1, LAYOUT_VERSION, &chat1, &chat1.buckets[0]);
        let fp2 = compute_fingerprint(1, LAYOUT_VERSION, &chat2, &chat2.buckets[0]);
        assert_ne!(fp1, fp2);
    }

    #[test]
    fn renders_basic_text_item_with_reaction() {
        let profile = RenderProfile {
            provider: Provider::Test,
            source_label: "Test".to_string(),
            chat_kind: "Test Chat".to_string(),
            message_kind: "Test Message".to_string(),
            reaction_kind: "Test Reaction".to_string(),
            chat_entity_kind: ENTITY_KIND_CONVERSATION,
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

    /// The header stays a real `##` — qmd scores an `h2` far above the
    /// blank line it would otherwise cut a chunk at, so dropping the
    /// heading for a plain `<div>` would quietly coarsen every chat's
    /// chunk boundaries.
    #[test]
    fn message_header_is_an_h2_with_a_hoverable_short_timestamp() {
        let chat = mk_chat();
        let md = render_markdown(
            &test_profile(),
            &chat,
            &chat.buckets[0],
            "Test · Bridge Crew",
            "fp",
        );
        assert!(
            md.contains("## <span class=\"msg-author\">Picard</span> "),
            "{md}"
        );
        assert!(
            md.contains(
                "<time class=\"msg-ts\" datetime=\"2364-04-11T00:00:00+00:00\" \
                 title=\"2364-04-11 00:00:00 UTC\">Sat Apr 11th, 2364 at 00:00</time>"
            ),
            "{md}"
        );
    }

    #[test]
    fn an_author_named_in_markup_cannot_break_out_of_the_header() {
        let mut chat = mk_chat();
        chat.buckets[0].items[0].author_display = "<script>x</script> & co".to_string();
        let md = render_markdown(
            &test_profile(),
            &chat,
            &chat.buckets[0],
            "Test · Bridge Crew",
            "fp",
        );
        assert!(
            md.contains("&lt;script&gt;x&lt;/script&gt; &amp; co"),
            "{md}"
        );
        assert!(!md.contains("<script>"), "{md}");
    }

    fn aside_item(uuid: &str, text: &str) -> NormalizedChatItem {
        NormalizedChatItem {
            message_uuid: uuid.to_string(),
            author_id: "tool".to_string(),
            author_display: "tool".to_string(),
            date_ms: Some(12442118400000),
            text: Some(text.to_string()),
            kind: ItemKind::Text,
            attachments: vec![],
            reactions: vec![],
            system_note: None,
            source_url: None,
            kind_label: Some("Tool Call".to_string()),
            source_ref: None,
            is_aside: true,
        }
    }

    /// One `<details>` per *run*, not per aside — the whole point is
    /// that a turn's five tool steps cost the reader one line.
    #[test]
    fn adjacent_asides_share_one_collapsed_details() {
        let mut chat = mk_chat();
        let spoken = chat.buckets[0].items[0].clone();
        chat.buckets[0].items = vec![
            spoken.clone(),
            aside_item("aside-1", "first tool"),
            aside_item("aside-2", "second tool"),
            spoken,
            aside_item("aside-3", "third tool"),
        ];
        let md = render_markdown(
            &test_profile(),
            &chat,
            &chat.buckets[0],
            "Test · Bridge Crew",
            "fp",
        );

        assert_eq!(
            md.matches("<details class=\"tool-group\">").count(),
            2,
            "two runs, two wrappers: {md}"
        );
        assert!(md.contains("<summary>🛠 2 tool steps</summary>"), "{md}");
        assert!(md.contains("<summary>🛠 1 tool step</summary>"), "{md}");
        // Every aside keeps its own anchor inside the wrapper, so a
        // grid row still has something to scroll to.
        for uuid in ["aside-1", "aside-2", "aside-3"] {
            assert!(md.contains(&format!("id=\"m-{uuid}\"")), "{md}");
        }
    }

    #[test]
    fn flipping_is_aside_re_renders_the_document() {
        let chat1 = mk_chat();
        let mut chat2 = mk_chat();
        chat2.buckets[0].items[0].is_aside = true;
        assert_ne!(
            compute_fingerprint(1, LAYOUT_VERSION, &chat1, &chat1.buckets[0]),
            compute_fingerprint(1, LAYOUT_VERSION, &chat2, &chat2.buckets[0]),
        );
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
            provider: Provider::Test,
            source_label: "Test".to_string(),
            chat_kind: "Test Chat".to_string(),
            message_kind: "Test Message".to_string(),
            reaction_kind: "Test Reaction".to_string(),
            chat_entity_kind: ENTITY_KIND_CONVERSATION,
            render_version: 1,
        };
        let md = render_markdown(&profile, &chat, &chat.buckets[0], "Test", "fp");
        assert!(md.contains("not yet fetched"));
        assert!(md.contains("https://example/vscapture"));
    }

    #[test]
    fn chat_source_url_surfaces_in_title_and_chat_grid_row() {
        let profile = RenderProfile {
            provider: Provider::Test,
            source_label: "Test".to_string(),
            chat_kind: "Test Chat".to_string(),
            message_kind: "Test Message".to_string(),
            reaction_kind: "Test Reaction".to_string(),
            chat_entity_kind: ENTITY_KIND_CONVERSATION,
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
        let rows = rows_of(&profile, &chat);
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
            compute_fingerprint(1, LAYOUT_VERSION, &none, &none.buckets[0]),
            compute_fingerprint(1, LAYOUT_VERSION, &bare, &bare.buckets[0]),
        );
        // …while setting / changing it re-cuts the fingerprint.
        let mut set = mk_chat();
        set.source_url = Some("https://example.com/a".to_string());
        assert_ne!(
            compute_fingerprint(1, LAYOUT_VERSION, &none, &none.buckets[0]),
            compute_fingerprint(1, LAYOUT_VERSION, &set, &set.buckets[0]),
        );
    }

    #[test]
    fn title_override_replaces_derived_heading() {
        let profile = RenderProfile {
            provider: Provider::Test,
            source_label: "Test".to_string(),
            chat_kind: "Test Chat".to_string(),
            message_kind: "Test Message".to_string(),
            reaction_kind: "Test Reaction".to_string(),
            chat_entity_kind: ENTITY_KIND_CONVERSATION,
            render_version: 1,
        };
        let mut chat = mk_chat();
        chat.title = Some("#bridge: Make it so.".to_string());

        // render_one builds the title; render_markdown takes it as a param,
        // so exercise the heading logic via render_one's formatting here by
        // re-deriving the same way render_one does.
        let chat_title = match &chat.title {
            Some(t) => t.clone(),
            None => format!("{} · {}", profile.source_label, chat.display),
        };
        assert_eq!(chat_title, "#bridge: Make it so.");

        // And a set title re-cuts the fingerprint (None stays stable).
        let plain = mk_chat();
        assert_ne!(
            compute_fingerprint(1, LAYOUT_VERSION, &plain, &plain.buckets[0]),
            compute_fingerprint(1, LAYOUT_VERSION, &chat, &chat.buckets[0]),
        );
    }

    #[test]
    fn per_message_source_url_surfaces_in_header_and_grid_row() {
        let profile = RenderProfile {
            provider: Provider::Test,
            source_label: "Test".to_string(),
            chat_kind: "Test Chat".to_string(),
            message_kind: "Test Message".to_string(),
            reaction_kind: "Test Reaction".to_string(),
            chat_entity_kind: ENTITY_KIND_CONVERSATION,
            render_version: 1,
        };
        let mut chat = mk_chat();
        chat.buckets[0].items[0].source_url = Some("https://slack.example/p123".to_string());

        // Message header carries a `↗` linkout.
        let md = render_markdown(&profile, &chat, &chat.buckets[0], "Test", "fp");
        assert!(
            md.contains("class=\"source-link\"") && md.contains("https://slack.example/p123"),
            "message header carries the per-message linkout: {md}"
        );

        // The message-level grid row (row[1], after the chat row) carries it.
        let rows = rows_of(&profile, &chat);
        let msg = rows
            .iter()
            .find(|r| r.kind == profile.message_kind)
            .unwrap();
        assert_eq!(
            msg.source_url.as_deref(),
            Some("https://slack.example/p123")
        );

        // A per-message URL re-cuts the fingerprint.
        let plain = mk_chat();
        assert_ne!(
            compute_fingerprint(1, LAYOUT_VERSION, &plain, &plain.buckets[0]),
            compute_fingerprint(1, LAYOUT_VERSION, &chat, &chat.buckets[0]),
        );
    }

    #[test]
    fn kind_label_overrides_message_kind_in_grid_row() {
        let profile = RenderProfile {
            provider: Provider::Test,
            source_label: "Test".to_string(),
            chat_kind: "Test Chat".to_string(),
            message_kind: "Test Message".to_string(),
            reaction_kind: "Test Reaction".to_string(),
            chat_entity_kind: ENTITY_KIND_CONVERSATION,
            render_version: 1,
        };
        let mut chat = mk_chat();
        chat.buckets[0].items[0].kind_label = Some("LLM Response".to_string());

        let rows = rows_of(&profile, &chat);
        // The message row uses the override, not the profile default.
        assert!(rows.iter().any(|r| r.kind == "LLM Response"));
        assert!(!rows.iter().any(|r| r.kind == "Test Message"));

        // …and it re-cuts the fingerprint.
        let plain = mk_chat();
        assert_ne!(
            compute_fingerprint(1, LAYOUT_VERSION, &plain, &plain.buckets[0]),
            compute_fingerprint(1, LAYOUT_VERSION, &chat, &chat.buckets[0]),
        );
    }

    fn test_profile() -> RenderProfile {
        RenderProfile {
            provider: Provider::Test,
            source_label: "Test".to_string(),
            chat_kind: "Test Chat".to_string(),
            message_kind: "Test Message".to_string(),
            reaction_kind: "Test Reaction".to_string(),
            chat_entity_kind: ENTITY_KIND_CONVERSATION,
            render_version: 1,
        }
    }

    /// The bug this whole `Option<i64>` change exists to remove: an item
    /// upstream never stamped used to land in the grid as a real-looking
    /// `1970-01-01T00:00:00+00:00`. It must be null instead.
    #[test]
    fn undated_item_gets_a_null_when_ts_not_the_epoch() {
        let profile = test_profile();
        let mut chat = mk_chat();
        chat.buckets[0].items[0].date_ms = None;
        chat.buckets[0].items[0].reactions[0].date_ms = None;

        let rows = rows_of(&profile, &chat);
        for r in &rows {
            assert_eq!(
                r.when_ts, None,
                "{} row fabricated a timestamp: {:?}",
                r.kind, r.when_ts
            );
        }
        assert!(
            !rows
                .iter()
                .any(|r| r.when_ts.as_deref().is_some_and(|t| t.starts_with("1970"))),
            "no row may carry an epoch stand-in",
        );
    }

    /// An empty bucket is reachable — `render_markdown` renders it as
    /// "_(no messages)_" — and used to hand its chat-level row a 1970
    /// stamp.
    #[test]
    fn empty_bucket_chat_row_has_no_timestamp() {
        let profile = test_profile();
        let mut chat = mk_chat();
        chat.buckets[0].items.clear();

        let rows = rows_of(&profile, &chat);
        assert_eq!(rows.len(), 1, "only the chat-level row");
        assert_eq!(rows[0].when_ts, None);
    }

    /// A dated bucket is unaffected, and a bucket whose *first* item is
    /// undated still takes the earliest real stamp rather than a null.
    #[test]
    fn chat_row_takes_the_earliest_real_timestamp() {
        let profile = test_profile();
        let mut chat = mk_chat();
        let dated = chat.buckets[0].items[0].clone();
        let mut undated = dated.clone();
        undated.message_uuid = "55555555-5555-5555-5555-555555555555".to_string();
        undated.date_ms = None;
        undated.reactions.clear();
        // Undated first, the order every provider's `sort_by_key` gives
        // (`None < Some`).
        chat.buckets[0].items = vec![undated, dated];

        let rows = rows_of(&profile, &chat);
        assert_eq!(
            rows[0].when_ts.as_deref(),
            Some("2364-04-11T00:00:00+00:00"),
            "chat row keeps the bucket's earliest real stamp",
        );
    }

    #[test]
    fn undated_item_renders_words_not_a_fake_date_in_markdown() {
        let profile = test_profile();
        let mut chat = mk_chat();
        chat.buckets[0].items[0].date_ms = None;
        let md = render_markdown(&profile, &chat, &chat.buckets[0], "Test", "fp");
        assert!(md.contains("(no timestamp)"), "{md}");
        assert!(!md.contains("1970"), "{md}");
    }

    /// `None` and `Some(0)` are different facts and must not collide in
    /// the fingerprint, or an item that gains or loses a stamp never
    /// re-renders.
    #[test]
    fn fingerprint_separates_no_timestamp_from_the_epoch() {
        let mut undated = mk_chat();
        undated.buckets[0].items[0].date_ms = None;
        let mut epoch = mk_chat();
        epoch.buckets[0].items[0].date_ms = Some(0);
        assert_ne!(
            compute_fingerprint(1, LAYOUT_VERSION, &undated, &undated.buckets[0]),
            compute_fingerprint(1, LAYOUT_VERSION, &epoch, &epoch.buckets[0]),
        );
    }

    #[test]
    fn org_columns_populate_every_grid_row() {
        let profile = RenderProfile {
            provider: Provider::Test,
            source_label: "Test".to_string(),
            chat_kind: "Test Chat".to_string(),
            message_kind: "Test Message".to_string(),
            reaction_kind: "Test Reaction".to_string(),
            chat_entity_kind: ENTITY_KIND_CONVERSATION,
            render_version: 1,
        };
        let mut chat = mk_chat();
        chat.org_uuid = Some("org-123".to_string());
        chat.org_name = Some("Starfleet".to_string());

        let rows = rows_of(&profile, &chat);
        // chat, message, and reaction rows all carry org_uuid/org_name.
        assert!(rows.len() >= 3);
        for r in &rows {
            assert_eq!(r.org_uuid.as_deref(), Some("org-123"));
            assert_eq!(r.org_name.as_deref(), Some("Starfleet"));
        }

        // org identity folds into the fingerprint (only when set).
        let plain = mk_chat();
        assert_ne!(
            compute_fingerprint(1, LAYOUT_VERSION, &plain, &plain.buckets[0]),
            compute_fingerprint(1, LAYOUT_VERSION, &chat, &chat.buckets[0]),
        );
    }
}
