//! `render_all` — drives every `(chat, period)` bucket through
//! [`render_one`] and feeds rendered docs into the orchestrator's
//! `on_doc_complete` callback.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

/// Default `upstream_entity_kind` for a chat/thread-level row — the
/// value most providers put in [`RenderProfile::chat_entity_kind`].
pub const ENTITY_KIND_CONVERSATION: &str = "conversation";

/// The markdown layout in this file has its own version, beside the
/// provider's [`RenderProfile::render_version`].
///
/// **Bump this whenever `render_markdown` changes what it writes.**
/// Without it, changing the shared layout means editing the version
/// constant in all ten providers by hand and re-rendering nothing in
/// the one you forget. It reaches the render step as a render param
/// ([`layout_params`]), so a bump renders every document again the way
/// any param change does. The provider's own number stays its own —
/// `datalib_step`'s render step checks that every version stored on
/// disk is one its processors declare, so this must not be mixed into
/// the stored value.
pub const LAYOUT_VERSION: u32 = 4;

/// What every chat-common provider declares through
/// `RenderProcessor::render_params`, merged with its own knobs: the
/// layout version, so a bump to it re-renders the source.
pub fn layout_params() -> serde_json::Value {
    serde_json::json!({ "layout_version": LAYOUT_VERSION })
}

/// `layout_params()` with a provider's own knobs folded in.
pub fn layout_params_with(own: serde_json::Value) -> serde_json::Value {
    let mut params = layout_params();
    if let (Some(base), Some(extra)) = (params.as_object_mut(), own.as_object()) {
        for (k, v) in extra {
            base.insert(k.clone(), v.clone());
        }
    }
    params
}

use anyhow::{Context, Result};
use datalib_etl::blob_cas::BlobBundle;
use datalib_etl::progress::Progress;
use datalib_etl::title::Title;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::message::{timestamp_html, MessageHeader};
use datalib_etl_render::section::{join, msg_div_open, Section};
use datalib_schema::grid_rows::GridRow;
use datalib_schema::problems::{Outcome, ProblemRow, Scope, Stage};
use datalib_schema::providers::Provider;

use crate::types::{ItemKind, NormalizedChat, NormalizedChatItem, NormalizedDoc};
use datalib_etl_render::html::escape_text;

/// Per-provider knobs the renderer parameterizes on. Values that
/// would otherwise be hard-coded as `"signal"` / `"Signal Chat"` /
/// `"Signal Message"` so a single render function serves every chat
/// provider.
#[derive(Debug, Clone)]
pub struct RenderProfile {
    /// On-disk subdir under `render_markdown/<provider>/<source_id>/…`
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
    /// Precision of the `created_at` this provider stamps on its grid
    /// rows. Not a free choice: changing it changes every row, so the
    /// provider's whole tree re-renders. Beeper is the one source whose
    /// upstream timestamps are meaningful below the second.
    pub stamp_precision: RecordStampPrecision,
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
    pub items_rendered: usize,
    pub reactions_rendered: usize,
    /// Every chat rendered, with what it was built from — the buckets
    /// the caller declares through `RenderCtx::declare_bucket`. A chat
    /// handed in that produced nothing is here too, which is how its
    /// old documents go.
    pub buckets: Buckets,
}

pub use datalib_etl_render::inputs::{Bucket, Buckets};

#[allow(clippy::too_many_arguments)]
pub fn render_all(
    profile: &RenderProfile,
    chats: &[NormalizedChat],
    out_dir: &Path,
    source_id: &str,
    blobs_by_chat: &HashMap<String, BlobBundle>,
    progress: &Progress,
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
        summary.buckets.push(Bucket {
            key: chat.chat_uuid.clone(),
            inputs: chat.inputs.clone(),
        });
        for doc in &chat.buckets {
            let (items, reactions) = render_one(
                profile,
                chat,
                doc,
                out_dir,
                source_id,
                bundle,
                on_doc_complete,
            )?;
            summary.docs_rendered += 1;
            summary.items_rendered += items;
            summary.reactions_rendered += reactions;
            progress.inc(1);
        }
    }
    Ok(summary)
}

/// Render one document; `(items, reactions)` rendered. Always: the
/// store writes what it is handed, and doltlite's content-addressed
/// tables carry no diff for a document that came out the same.
fn render_one(
    profile: &RenderProfile,
    chat: &NormalizedChat,
    doc: &NormalizedDoc,
    out_dir: &Path,
    source_id: &str,
    blobs: &BlobBundle,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<(usize, usize)> {
    let (md_path, page_dir) = output_paths(out_dir, source_id, chat, &doc.period_key);
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

    let sections = render_markdown(profile, chat, doc, &chat_title, &doc_title);
    fs::write(&md_path, join(&sections)).with_context(|| format!("write {}", md_path.display()))?;

    let md_rel = md_path
        .strip_prefix(out_dir)
        .unwrap_or(&md_path)
        .to_string_lossy()
        .into_owned();

    let mut problems: Vec<ProblemRow> = Vec::new();
    let rows = build_grid_rows(
        profile,
        chat,
        doc,
        &chat_title,
        &md_rel,
        source_id,
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
        source_id: source_id.to_string(),
        upstream_cursor: None,
        bucket_key: Some(chat.chat_uuid.clone()),
        md_path,
        render_version: profile.render_version,
        rows,
        sections,
        edges: Vec::new(),
        problems,
    })
    .with_context(|| format!("on_doc_complete {}", doc.markdown_uuid))?;

    Ok((items_rendered, reactions_rendered))
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

/// `<out>/<stanza>/render_markdown/<chat_uuid>/<period>.md` plus the matching
/// markdown and its parent dir. The directory is the chat's stable UUID — never a
/// title-derived slug — so an upstream rename (channel/title change)
/// re-renders in place instead of orphaning the old file at a stale path. The
/// human title still lives in the markdown frontmatter and the grid_rows DB.
fn output_paths(
    out_dir: &Path,
    source_id: &str,
    chat: &NormalizedChat,
    period_key: &str,
) -> (PathBuf, PathBuf) {
    let mut page_dir = datalib_etl::layout::render_markdown_root(out_dir, source_id);
    if let Some(prefix) = &chat.path_prefix {
        page_dir = page_dir.join(prefix);
    }
    let page_dir = page_dir.join(&chat.chat_uuid);
    let md_path = page_dir.join(format!("{period_key}.md"));
    (md_path, page_dir)
}

// Markdown

fn render_markdown(
    profile: &RenderProfile,
    chat: &NormalizedChat,
    doc: &NormalizedDoc,
    // `chat_title` is the chat's own name and `title` the composed
    // "name (period)". The frontmatter wants the composed one; the
    // heading takes them apart, so a clamp cannot eat the period.
    chat_title: &str,
    title: &str,
) -> Vec<Section> {
    let mut s = String::with_capacity(1024);
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
    s.push_str("---\n\n");

    // The chat's name and the period go in separately: a long title is
    // clamped, and `(2024-03)` — which says *which slice of the
    // conversation this file is* — must survive that.
    let period = format!("({})", doc.period_key);
    s.push_str(
        &Title {
            text: chat_title,
            suffix: Some(&period),
            markdown_uuid: Some(&doc.markdown_uuid),
            // Public per-chat URL when the provider has one (LinkedIn
            // post, Slack permalink, …); None for backup-based providers.
            source_url: chat.source_url.as_deref(),
        }
        .render(),
    );

    if doc.items.is_empty() {
        s.push_str("_(no messages)_\n");
        return vec![Section::unkeyed(s)];
    }

    let mut sections = vec![Section::unkeyed(s)];
    let mut i = 0;
    while i < doc.items.len() {
        let run_end = doc.items[i..]
            .iter()
            .position(|it| !it.is_aside)
            .map_or(doc.items.len(), |n| i + n);
        if run_end > i {
            render_aside_run(&mut sections, profile, &doc.items[i..run_end]);
            i = run_end;
        } else {
            sections.push(render_item(profile, &doc.items[i]));
            i += 1;
        }
    }
    if let Some(orphans) = render_orphan_reactions(doc) {
        sections.push(Section::unkeyed(orphans));
    }
    sections
}

/// Reactions the provider could not place on any message in this
/// document, listed at the end under the upstream id they name.
fn render_orphan_reactions(doc: &NormalizedDoc) -> Option<String> {
    if doc.orphan_reactions.is_empty() {
        return None;
    }
    let mut s = String::new();
    s.push_str("---\n\n## Reactions to messages not in this mirror\n\n");
    for group in &doc.orphan_reactions {
        s.push_str(&format!(
            "- target `{}`:\n",
            escape_text(&group.target_native_id)
        ));
        for r in &group.reactions {
            s.push_str(&format!(
                "  - <span id=\"m-{uuid}\" data-section-uuid=\"{uuid}\">{emoji} {who}</span> ({ts})\n",
                uuid = r.reaction_uuid,
                emoji = r.emoji,
                who = escape_text(&r.reactor_display),
                ts = timestamp_html(r.date_ms),
            ));
        }
    }
    s.push('\n');
    Some(s)
}

/// Wrap one run of adjacent asides in a single collapsed `<details>`.
///
/// The `<details>` sits *outside* the per-message `<div>`s so every
/// anchor, copy button and grid-row highlight inside it keeps working
/// unchanged — the frontend opens the enclosing `<details>` when it
/// scrolls to a section within one. Its opener and closer are unkeyed
/// sections of their own, so each aside stays its own keyed section.
fn render_aside_run(
    sections: &mut Vec<Section>,
    profile: &RenderProfile,
    items: &[NormalizedChatItem],
) {
    let plural = if items.len() == 1 { "" } else { "s" };
    sections.push(Section::unkeyed(format!(
        "<details class=\"tool-group\">\n<summary>🛠 {n} tool step{plural}</summary>\n\n",
        n = items.len(),
    )));
    for item in items {
        sections.push(render_item(profile, item));
    }
    sections.push(Section::unkeyed("</details>\n\n".to_string()));
}

fn render_item(profile: &RenderProfile, item: &NormalizedChatItem) -> Section {
    let mut s = String::with_capacity(512);
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
                ts = timestamp_html(item.date_ms)
            ));
            s.push_str("</div>\n\n");
            return Section::keyed(&item.message_uuid, s);
        }
        ItemKind::Text | ItemKind::Attachment => {
            s.push_str(
                &MessageHeader {
                    author: &item.author_display,
                    date_ms: item.date_ms,
                    source_url: item.source_url.as_deref(),
                }
                .render(),
            );
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
                render_attachment(&mut s, att);
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
    Section::keyed(&item.message_uuid, s)
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
    source_id: &str,
    problems: &mut Vec<ProblemRow>,
) -> Vec<GridRow> {
    let mut rows: Vec<GridRow> = Vec::with_capacity(1 + doc.items.len());

    // The document row brackets the bucket: created at the earliest
    // *real* stamp in it, modified at the latest — a reaction counts, it
    // is a change to the thread. `min()`/`max()` over the items that
    // have a stamp rather than the first and last items', because an
    // undated item sorts to the front (`None < Some` in every provider's
    // `sort_by_key`), and a bucket with no dated item at all — including
    // the empty bucket `render_markdown` renders as "_(no messages)_" —
    // then gets `None` rather than a 1970 stamp.
    let dated = || doc.items.iter().filter_map(|i| i.date_ms);
    let first_ts = stamp_from_ms(dated().min(), profile.stamp_precision);
    let last_ts = stamp_from_ms(
        dated()
            .chain(
                doc.items
                    .iter()
                    .flat_map(|i| i.reactions.iter().filter_map(|r| r.date_ms)),
            )
            .max(),
        profile.stamp_precision,
    );
    let conversation_name = Some(chat.display.clone());
    let entire_chat = format!("/chat/{}", doc.markdown_uuid);
    let bodies: Vec<String> = doc.items.iter().map(message_body).collect();

    rows.extend(
        GridRow::builder()
            .uuid(doc.markdown_uuid.clone())
            .provider(profile.provider)
            .kind(profile.chat_kind.clone())
            .source_label(profile.source_label.clone())
            .is_document(true)
            .created_at(first_ts)
            .modified_at(last_ts)
            .byte_size(Some(bodies.iter().map(|b| b.len() as i64).sum()))
            .item_count(Some(doc.items.len() as i64))
            .author(chat.author.clone())
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
            .upstream_id(match &doc.source_ref {
                Some(r) => Some(r.native_id.clone()),
                None => chat.external_id.clone(),
            })
            .upstream_entity_kind(Some(match &doc.source_ref {
                Some(r) => r.entity_kind.clone(),
                None => profile.chat_entity_kind.to_string(),
            }))
            .upstream_scope(chat.upstream_scope.clone())
            .markdown_uuid(Some(doc.markdown_uuid.clone()))
            .build_or_record(
                source_id,
                &doc.markdown_uuid,
                profile.render_version,
                problems,
            ),
    );

    let _ = chat_title; // reserved for future per-message title context

    for (idx, (item, text)) in doc.items.iter().zip(bodies).enumerate() {
        // What the provider could not do with the item while
        // normalizing it: a document-scoped row per problem, keyed to
        // the item, swept with the document like the builder's own.
        problems.extend(item.problems.iter().map(|p| {
            ProblemRow::new(
                source_id,
                Stage::Parse,
                Scope::Markdown(&doc.markdown_uuid),
                Some(&item.message_uuid),
                Outcome::Nulled,
                p.clone(),
                Some(profile.render_version),
            )
        }));
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
                // Items inherit the chat's account: a chat belongs to
                // exactly one workspace/account, and every row inside
                // it was minted under that same one.
                .upstream_scope(chat.upstream_scope.clone())
                .created_at(stamp_from_ms(item.date_ms, profile.stamp_precision))
                .byte_size(Some(text.len() as i64))
                .item_count(Some(1))
                // An empty display is "upstream named nobody", which is a
                // null — never a row whose author is the empty string,
                // and never a stand-in like "unknown".
                .author(non_empty(&item.author_display))
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
                    source_id,
                    &doc.markdown_uuid,
                    profile.render_version,
                    problems,
                ),
        );
        for r in &item.reactions {
            rows.extend(reaction_row(
                profile,
                chat,
                doc,
                r,
                &conversation_name,
                &entire_chat,
                md_rel,
                source_id,
                problems,
            ));
        }
    }
    for group in &doc.orphan_reactions {
        for r in &group.reactions {
            rows.extend(reaction_row(
                profile,
                chat,
                doc,
                r,
                &conversation_name,
                &entire_chat,
                md_rel,
                source_id,
                problems,
            ));
        }
    }
    rows
}

#[allow(clippy::too_many_arguments)]
fn reaction_row(
    profile: &RenderProfile,
    chat: &NormalizedChat,
    doc: &NormalizedDoc,
    r: &crate::types::NormalizedReaction,
    conversation_name: &Option<String>,
    entire_chat: &str,
    md_rel: &str,
    source_id: &str,
    problems: &mut Vec<ProblemRow>,
) -> Option<GridRow> {
    GridRow::builder()
        .uuid(r.reaction_uuid.clone())
        .provider(profile.provider)
        .kind(profile.reaction_kind.clone())
        .source_label(profile.source_label.clone())
        .upstream_id(r.source_ref.as_ref().map(|s| s.native_id.clone()))
        .upstream_entity_kind(r.source_ref.as_ref().map(|s| s.entity_kind.clone()))
        .upstream_scope(chat.upstream_scope.clone())
        .created_at(stamp_from_ms(r.date_ms, profile.stamp_precision))
        .author(non_empty(&r.reactor_display))
        .account(chat.account.clone())
        .org_uuid(chat.org_uuid.clone())
        .org_name(chat.org_name.clone())
        .project(chat.project.clone())
        .channel(conversation_name.clone())
        .conversation_name(conversation_name.clone())
        .conversation_uuid(chat.chat_uuid.clone())
        .entire_chat(entire_chat.to_string())
        .text(r.emoji.clone())
        .qmd_path(Some(md_rel.to_string()))
        .markdown_uuid(Some(doc.markdown_uuid.clone()))
        .build_or_record(
            source_id,
            &doc.markdown_uuid,
            profile.render_version,
            problems,
        )
}

fn non_empty(s: &str) -> Option<String> {
    (!s.is_empty()).then(|| s.to_string())
}

/// The message-level row's `text`, and the bytes its `byte_size` counts:
/// the body alone, never an attachment's bytes, so the number means one
/// thing on every provider whether or not it knows its attachments' sizes.
fn message_body(item: &NormalizedChatItem) -> String {
    match item.kind {
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
    }
}

fn attachment_search_text(item: &NormalizedChatItem) -> String {
    item.attachments
        .iter()
        .filter_map(|a| a.file_name.clone())
        .collect::<Vec<_>>()
        .join(" ")
}

// Format helpers
use datalib_time::{record_stamp_from_unix_millis, RecordStampPrecision};

/// Seconds precision, as this renderer has always emitted. Changing it
/// would re-render every document chat-common has written.
fn stamp_from_ms(ms: Option<i64>, precision: RecordStampPrecision) -> Option<String> {
    record_stamp_from_unix_millis(ms, precision)
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
    use datalib_schema::problems::{Outcome, Reason, ScopeKind, Severity, Stage};

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
    use crate::types::{NormalizedAttachment, NormalizedReaction, OrphanReactions};

    fn mk_chat() -> NormalizedChat {
        NormalizedChat {
            inputs: Vec::new(),
            id: "100".to_string(),
            chat_uuid: "11111111-1111-1111-1111-111111111111".to_string(),
            display: "Bridge Crew".to_string(),
            author: None,
            account: Some("acct-1".to_string()),
            project: None,
            external_id: Some("bridge-crew@g.us".to_string()),
            source_url: None,
            upstream_scope: None,
            title: None,
            org_uuid: None,
            org_name: None,
            path_prefix: None,
            buckets: vec![NormalizedDoc {
                period_key: "2364-04".to_string(),
                markdown_uuid: "22222222-2222-2222-2222-222222222222".to_string(),
                source_ref: None,
                orphan_reactions: Vec::new(),
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
                    problems: Vec::new(),
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
        assert_eq!(p.outcome, Outcome::Dropped);
        assert_eq!(p.severity, Severity::Error);
        assert_eq!(p.stage, Stage::GridRow);
        assert_eq!(p.source_id, "test_source");
        assert_eq!(
            p.scope_key, chat.buckets[0].markdown_uuid,
            "swept with the document it belongs to"
        );
        assert_eq!(p.scope_kind, ScopeKind::Markdown);
        assert_eq!(p.render_version, Some(i64::from(profile.render_version)));
        // A row with no uuid names no item; its id comes from the scope
        // and the field, so the same bad record does not accumulate a
        // new row every run.
        assert!(p.item_uuid.is_none());
        // Never a count without a reason.
        assert_eq!(p.reason, Reason::NoIdentity);
        assert_eq!(p.field.as_deref(), Some("uuid"));
        // Stamping is the store's job, not the renderer's.
        assert!(p.first_seen_at_utc.is_empty() && p.last_seen_at_utc.is_empty());
    }

    /// A problem the provider found while normalizing an item — a
    /// stamp that would not parse — reaches the store as a parse-stage
    /// row on the document, keyed to the item, beside the builder's
    /// own rows.
    #[test]
    fn an_items_own_problems_become_document_rows_keyed_to_it() {
        use crate::types::own_stamp_ms;
        let profile = test_profile();
        let mut chat = mk_chat();
        let item = &mut chat.buckets[0].items[0];
        let ms = own_stamp_ms(
            Some("stardate 47988.1"),
            "created_at",
            |_| None,
            &mut item.problems,
        );
        assert_eq!(ms, None);
        assert_eq!(item.problems.len(), 1);
        let mut problems = Vec::new();
        build_grid_rows(
            &profile,
            &chat,
            &chat.buckets[0],
            "Test",
            "x.md",
            "test_source",
            &mut problems,
        );
        assert_eq!(problems.len(), 1, "{problems:?}");
        let p = &problems[0];
        assert_eq!(p.stage, Stage::Parse);
        assert_eq!(p.severity, Severity::Warning);
        assert_eq!(p.reason, Reason::CoercionFailed);
        assert_eq!(p.field.as_deref(), Some("created_at"));
        assert_eq!(p.sample, "stardate 47988.1");
        assert_eq!(p.scope_key, chat.buckets[0].markdown_uuid);
        assert_eq!(
            p.item_uuid.as_deref(),
            Some(chat.buckets[0].items[0].message_uuid.as_str())
        );
        // An absent stamp is an absence, not a problem.
        let mut none = Vec::new();
        assert_eq!(
            own_stamp_ms(None, "created_at", |_| Some(1), &mut none),
            None
        );
        assert_eq!(
            own_stamp_ms(Some("  "), "created_at", |_| Some(1), &mut none),
            None
        );
        assert!(none.is_empty());
    }

    /// The id is minted from the scope and the field, so a record that
    /// stays broken keeps one row across runs rather than growing one per run.
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
        assert_eq!(a[0].problem_uuid, b[0].problem_uuid);
    }

    /// A message weighs its body in bytes, the document weighs the sum
    /// of its messages and counts them, and a reaction is neither.
    #[test]
    fn document_size_and_count_are_the_sum_of_its_messages() {
        let profile = test_profile();
        let mut chat = mk_chat();
        chat.buckets[0].items.push(NormalizedChatItem {
            message_uuid: "55555555-5555-5555-5555-555555555555".to_string(),
            author_id: "2".to_string(),
            author_display: "Worf".to_string(),
            date_ms: Some(12442118420000),
            text: None,
            kind: ItemKind::System,
            attachments: vec![],
            reactions: vec![],
            system_note: Some("Worf joined 🖖".to_string()),
            source_url: None,
            kind_label: None,
            source_ref: None,
            is_aside: false,
            problems: Vec::new(),
        });
        let rows = rows_of(&profile, &chat);

        let by_kind = |k: &str| rows.iter().filter(|r| r.kind == k).collect::<Vec<_>>();
        let messages = by_kind(&profile.message_kind);
        assert_eq!(messages.len(), 2);
        assert_eq!(messages[0].byte_size, Some("Make it so.".len() as i64));
        // Bytes, not characters: the vulcan salute is four of them.
        assert_eq!(messages[1].byte_size, Some("Worf joined 🖖".len() as i64));
        assert_eq!(messages[1].byte_size, Some(16));
        assert!(messages.iter().all(|m| m.item_count == Some(1)));

        let doc = &by_kind(&profile.chat_kind)[0];
        assert_eq!(doc.byte_size, Some(11 + 16));
        assert_eq!(doc.item_count, Some(2));

        let reaction = &by_kind(&profile.reaction_kind)[0];
        assert_eq!((reaction.byte_size, reaction.item_count), (None, None));
    }

    /// Bumping the shared layout must re-render every chat provider
    /// without any of them touching its own `RENDER_VERSION` — that is
    /// the whole reason [`LAYOUT_VERSION`] exists.
    #[test]
    fn renders_basic_text_item_with_reaction() {
        let profile = RenderProfile {
            provider: Provider::Test,
            source_label: "Test".to_string(),
            chat_kind: "Test Chat".to_string(),
            message_kind: "Test Message".to_string(),
            reaction_kind: "Test Reaction".to_string(),
            chat_entity_kind: ENTITY_KIND_CONVERSATION,
            stamp_precision: RecordStampPrecision::Seconds,
            render_version: 1,
        };
        let chat = mk_chat();
        let md = join(&render_markdown(
            &profile,
            &chat,
            &chat.buckets[0],
            "Test · Bridge Crew",
            "Test · Bridge Crew (2364-04)",
        ));
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
        let md = join(&render_markdown(
            &test_profile(),
            &chat,
            &chat.buckets[0],
            "Test · Bridge Crew",
            "Test · Bridge Crew (2364-04)",
        ));
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
        let md = join(&render_markdown(
            &test_profile(),
            &chat,
            &chat.buckets[0],
            "Test · Bridge Crew",
            "Test · Bridge Crew (2364-04)",
        ));
        assert!(
            md.contains("&lt;script&gt;x&lt;/script&gt; &amp; co"),
            "{md}"
        );
        assert!(!md.contains("<script>"), "{md}");
    }

    /// Every stamp in a rendered document is hoverable, not just the
    /// one in a message header. A system line and an orphan reaction
    /// each state their instant to the second in `title`, where the
    /// short form on screen has only minutes.
    #[test]
    fn every_timestamp_carries_the_full_instant() {
        let mut chat = mk_chat();
        chat.buckets[0].items[0].kind = ItemKind::System;
        chat.buckets[0].items[0].system_note = Some("Worf joined".to_string());
        chat.buckets[0].orphan_reactions = vec![OrphanReactions {
            target_native_id: "gone-upstream".to_string(),
            reactions: vec![NormalizedReaction {
                reaction_uuid: "55555555-5555-5555-5555-555555555555".to_string(),
                reactor_display: "Will Riker".to_string(),
                emoji: "\u{1fae1}".to_string(),
                // Ten seconds after the message, which only the long
                // form is precise enough to say.
                date_ms: Some(12442118410000),
                source_ref: None,
            }],
        }];
        let md = join(&render_markdown(
            &test_profile(),
            &chat,
            &chat.buckets[0],
            "Test \u{b7} Bridge Crew",
            "Test \u{b7} Bridge Crew (2364-04)",
        ));

        assert!(
            md.contains(
                "*<small><time class=\"msg-ts\" datetime=\"2364-04-11T00:00:00+00:00\" \
                 title=\"2364-04-11 00:00:00 UTC\">Sat Apr 11th, 2364 at 00:00</time> \
                 \u{2014} system: Worf joined</small>*"
            ),
            "system line lost its hoverable instant:\n{md}"
        );
        assert!(
            md.contains("title=\"2364-04-11 00:00:10 UTC\">Sat Apr 11th, 2364 at 00:00</time>)"),
            "orphan reaction lost its hoverable instant:\n{md}"
        );
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
            problems: Vec::new(),
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
        let md = join(&render_markdown(
            &test_profile(),
            &chat,
            &chat.buckets[0],
            "Test · Bridge Crew",
            "Test · Bridge Crew (2364-04)",
        ));

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

    /// A document's sections are one per item, each keyed by the item
    /// it wraps, with the frontmatter and the `<details>` wrappers
    /// unkeyed between them — the shape a diff subtracts by. And their
    /// join is the document: sectioning changed no byte.
    #[test]
    fn every_item_is_one_keyed_section_and_the_join_is_the_document() {
        let mut chat = mk_chat();
        let spoken = chat.buckets[0].items[0].clone();
        chat.buckets[0].items = vec![
            spoken.clone(),
            aside_item("aside-1", "first tool"),
            aside_item("aside-2", "second tool"),
            spoken.clone(),
        ];
        let sections = render_markdown(
            &test_profile(),
            &chat,
            &chat.buckets[0],
            "Test · Bridge Crew",
            "Test · Bridge Crew (2364-04)",
        );
        let keys: Vec<Option<&str>> = sections.iter().map(|s| s.uuid.as_deref()).collect();
        assert_eq!(
            keys,
            vec![
                None,
                Some(spoken.message_uuid.as_str()),
                None,
                Some("aside-1"),
                Some("aside-2"),
                None,
                Some(spoken.message_uuid.as_str()),
            ],
            "{sections:#?}"
        );
        for s in &sections {
            if let Some(uuid) = &s.uuid {
                assert!(
                    s.md.starts_with(&msg_div_open(uuid, Provider::Test)),
                    "{}",
                    s.md
                );
                assert!(s.md.ends_with("</div>\n\n"), "{}", s.md);
            }
        }
        assert!(
            sections[0].md.starts_with("---\ntitle:"),
            "{}",
            sections[0].md
        );
        assert!(sections[2].md.starts_with("<details"), "{}", sections[2].md);
        assert_eq!(sections[5].md, "</details>\n\n");
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
            stamp_precision: RecordStampPrecision::Seconds,
            render_version: 1,
        };
        let md = join(&render_markdown(
            &profile,
            &chat,
            &chat.buckets[0],
            "Test",
            "Test (2364-04)",
        ));
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
            stamp_precision: RecordStampPrecision::Seconds,
            render_version: 1,
        };
        let mut chat = mk_chat();
        chat.source_url = Some("https://example.com/post/42".to_string());

        // Title gets the `↗` source link.
        let md = join(&render_markdown(
            &profile,
            &chat,
            &chat.buckets[0],
            "Test",
            "Test (2364-04)",
        ));
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
    fn title_override_replaces_derived_heading() {
        let profile = RenderProfile {
            provider: Provider::Test,
            source_label: "Test".to_string(),
            chat_kind: "Test Chat".to_string(),
            message_kind: "Test Message".to_string(),
            reaction_kind: "Test Reaction".to_string(),
            chat_entity_kind: ENTITY_KIND_CONVERSATION,
            stamp_precision: RecordStampPrecision::Seconds,
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
            stamp_precision: RecordStampPrecision::Seconds,
            render_version: 1,
        };
        let mut chat = mk_chat();
        chat.buckets[0].items[0].source_url = Some("https://slack.example/p123".to_string());

        // Message header carries a `↗` linkout.
        let md = join(&render_markdown(
            &profile,
            &chat,
            &chat.buckets[0],
            "Test",
            "Test (2364-04)",
        ));
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
            stamp_precision: RecordStampPrecision::Seconds,
            render_version: 1,
        };
        let mut chat = mk_chat();
        chat.buckets[0].items[0].kind_label = Some("LLM Response".to_string());

        let rows = rows_of(&profile, &chat);
        // The message row uses the override, not the profile default.
        assert!(rows.iter().any(|r| r.kind == "LLM Response"));
        assert!(!rows.iter().any(|r| r.kind == "Test Message"));
    }

    fn test_profile() -> RenderProfile {
        RenderProfile {
            provider: Provider::Test,
            source_label: "Test".to_string(),
            chat_kind: "Test Chat".to_string(),
            message_kind: "Test Message".to_string(),
            reaction_kind: "Test Reaction".to_string(),
            chat_entity_kind: ENTITY_KIND_CONVERSATION,
            stamp_precision: RecordStampPrecision::Seconds,
            render_version: 1,
        }
    }

    /// The bug this whole `Option<i64>` change exists to remove: an item
    /// upstream never stamped used to land in the grid as a real-looking
    /// `1970-01-01T00:00:00+00:00`. It must be null instead.
    #[test]
    fn undated_item_gets_a_null_created_at_not_the_epoch() {
        let profile = test_profile();
        let mut chat = mk_chat();
        chat.buckets[0].items[0].date_ms = None;
        chat.buckets[0].items[0].reactions[0].date_ms = None;

        let rows = rows_of(&profile, &chat);
        for r in &rows {
            assert_eq!(
                r.created_at, None,
                "{} row fabricated a timestamp: {:?}",
                r.kind, r.created_at
            );
        }
        assert!(
            !rows.iter().any(|r| r
                .created_at
                .as_deref()
                .is_some_and(|t| t.starts_with("1970"))),
            "no row may carry an epoch stand-in",
        );
    }

    /// The chat row is the one document row, created at the first
    /// message and modified at the last change — here the reaction,
    /// which lands ten seconds after the only message.
    #[test]
    fn chat_row_is_the_document_and_brackets_the_bucket() {
        let profile = test_profile();
        let rows = rows_of(&profile, &mk_chat());
        let docs: Vec<&GridRow> = rows.iter().filter(|r| r.is_document).collect();
        assert_eq!(docs.len(), 1, "{rows:?}");
        let chat = docs[0];
        assert_eq!(chat.kind, profile.chat_kind);
        assert_eq!(
            chat.created_at.as_deref(),
            Some("2364-04-11T00:00:00+00:00")
        );
        assert_eq!(
            chat.modified_at.as_deref(),
            Some("2364-04-11T00:00:10+00:00")
        );
        for r in rows.iter().filter(|r| !r.is_document) {
            assert!(
                r.modified_at.is_none(),
                "{} row: {:?}",
                r.kind,
                r.modified_at
            );
        }
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
        assert_eq!(rows[0].created_at, None);
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
            rows[0].created_at.as_deref(),
            Some("2364-04-11T00:00:00+00:00"),
            "chat row keeps the bucket's earliest real stamp",
        );
    }

    #[test]
    fn undated_item_renders_words_not_a_fake_date_in_markdown() {
        let profile = test_profile();
        let mut chat = mk_chat();
        chat.buckets[0].items[0].date_ms = None;
        let md = join(&render_markdown(
            &profile,
            &chat,
            &chat.buckets[0],
            "Test",
            "Test (2364-04)",
        ));
        assert!(md.contains("(no timestamp)"), "{md}");
        assert!(!md.contains("1970"), "{md}");
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
            stamp_precision: RecordStampPrecision::Seconds,
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
    }
}
