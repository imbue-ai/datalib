//! Per-thread markdown rendering.
//!
//! Layout (page-dir, like notion / chatgpt / slack):
//!
//! ```text
//! rendered_md/jmap/<account_slug>/<thread_uuid>/
//!   index.md
//!   index.grid_rows.json
//!   blobs/<safe_filename>          ← one file per attachment referenced
//!                                    by any email in this thread
//! ```
//!
//! `index.md` carries YAML frontmatter with thread-level metadata
//! (subject, participants, mailbox labels, keywords, message count)
//! followed by one section per email in `receivedAt` order. Each
//! section's body is mail-parsed from the RFC 5322 `.eml` bytes
//! stored in the blob CAS — same code path for JMAP-sourced and
//! mbox-sourced messages — falling back to a short preview when
//! neither a text nor html body is present. Attachment links
//! resolve to the sibling `blobs/` directory; the byte-perfect copy
//! lives in the `blobs` doltlite table.
//!
//! The grid_rows sidecar emits two row kinds: one `"Email Thread"`
//! row for the thread itself + one `"Email"` row per email. Both
//! share `conversation_uuid = <thread_uuid>` so the existing
//! conversation_uuid filter machinery in the grid backend "just works".

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use frankweiler_etl::blob_cas::{blake3_hex, extension_for_content_type};
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl::render_cursor;
use frankweiler_index_lib::emit_sidecar;
use frankweiler_schema::grid_rows::GridRow;
use mail_parser::{Address, MessageParser, MimeHeaders, PartType};
use uuid::Uuid;

use super::parse::ParsedEmail;
use super::RENDER_VERSION;
use crate::extract::db::{LoadedAttachment, LoadedEmail};

/// A binary body part embedded in the .eml that the renderer needs
/// to materialize to disk. Two flavors:
///   * **cid-referenced**: the HTML body links it via
///     `<img src="cid:…">` → we rewrite to `blobs/<fname>` and htmd
///     emits the image inline.
///   * **loose inline**: a `multipart/mixed` or Apple-Mail-style
///     `multipart/appledouble` part with `Content-Disposition: inline`
///     and no Content-ID. The body has no in-text reference to it, so
///     we append `![](blobs/<fname>)` after the rendered body.
///
/// JMAP servers like Fastmail report `hasAttachment: false` for both
/// of these and leave them out of the `attachments` array — they're
/// classified as part of the body view. We discover them via
/// mail-parser at render time instead of relying on the join table.
#[derive(Default, Clone)]
struct InlinePart {
    cid: Option<String>,
    content_type: Option<String>,
    bytes: Vec<u8>,
}

/// Everything the renderer needs from the RFC 5322 `.eml` bytes,
/// extracted via mail-parser in one pass and made owned so we don't
/// hold the borrow across the whole render loop.
#[derive(Default)]
struct ParsedEml {
    from_display: String,
    /// Email addresses the message touches (from/to/cc), in insertion
    /// order. Used for the YAML `participants:` list at the thread
    /// level.
    participants: Vec<String>,
    /// Concatenated `text/plain` body parts.
    text_body: String,
    /// Concatenated `text/html` body parts.
    html_body: String,
    /// Non-text parts with a Content-ID. Materialized to `blobs/` at
    /// render time and linked via the existing cid→blob rewrite.
    inline_parts: Vec<InlinePart>,
}

impl ParsedEml {
    fn from_eml_bytes(bytes: &[u8]) -> Self {
        let Some(msg) = MessageParser::default().parse(bytes) else {
            return Self::default();
        };
        let from_display = format_address(msg.from());
        let mut participants_set: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();
        for addr in [msg.from(), msg.to(), msg.cc()].into_iter().flatten() {
            for a in addr.iter() {
                let email = a.address().unwrap_or_default().trim().to_string();
                if !email.is_empty() {
                    participants_set.insert(email);
                }
            }
        }
        let mut text_body = String::new();
        for &idx in &msg.text_body {
            if let Some(part) = msg.part(idx) {
                text_body.push_str(&part_text(part));
                text_body.push('\n');
            }
        }
        let mut html_body = String::new();
        for &idx in &msg.html_body {
            if let Some(part) = msg.part(idx) {
                html_body.push_str(&part_text(part));
                html_body.push('\n');
            }
        }
        let mut inline_parts: Vec<InlinePart> = Vec::new();
        for &idx in &msg.attachments {
            let Some(part) = msg.part(idx) else { continue };
            let bytes: Vec<u8> = match &part.body {
                PartType::Binary(b) | PartType::InlineBinary(b) => b.to_vec(),
                _ => continue,
            };
            let cid = part.content_id().map(str::to_string);
            let content_type = part.content_type().map(|ct| {
                let base = ct.ctype();
                match ct.subtype() {
                    Some(sub) => format!("{base}/{sub}"),
                    None => base.to_string(),
                }
            });
            inline_parts.push(InlinePart {
                cid,
                content_type,
                bytes,
            });
        }
        Self {
            from_display,
            participants: participants_set.into_iter().collect(),
            text_body,
            html_body,
            inline_parts,
        }
    }

    /// First ~200 chars of the plaintext (whitespace-collapsed), falling
    /// back to the HTML body when no text/plain part exists.
    fn preview(&self) -> String {
        let raw = if !self.text_body.trim().is_empty() {
            self.text_body.as_str()
        } else {
            self.html_body.as_str()
        };
        let collapsed: String = raw
            .chars()
            .map(|c| if c.is_whitespace() { ' ' } else { c })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ");
        collapsed.chars().take(200).collect()
    }
}

/// Stable on-disk name for an inline part: `<sha8>.<ext>` where the
/// hash is the first 16 hex of blake3(bytes) and the extension comes
/// from the content_type. Mirrors `BlobView::rendered_filename` so the
/// path-shape is uniform across "real" attachments and inline parts.
fn inline_blob_filename(bytes: &[u8], content_type: Option<&str>) -> String {
    let hash = blake3_hex(bytes);
    let short = &hash[..16.min(hash.len())];
    match extension_for_content_type(content_type) {
        Some(ext) => format!("{short}.{ext}"),
        None => short.to_string(),
    }
}

fn part_text(part: &mail_parser::MessagePart<'_>) -> String {
    match &part.body {
        PartType::Text(s) | PartType::Html(s) => s.to_string(),
        // mail-parser sometimes includes inline image parts in
        // `msg.html_body` (they're "part of" the HTML view). The
        // image bytes belong in the blob CAS, not the rendered
        // markdown — they render via the `<img src="cid:…">` →
        // `blobs/<file>` rewrite, not by being inlined as text.
        // Lossy-decoding multi-MB of JPEG/PNG into the body string
        // is what made htmd dump raw binary into index.md.
        _ => String::new(),
    }
}

fn format_address(addr: Option<&Address>) -> String {
    let Some(addr) = addr else {
        return "(unknown sender)".to_string();
    };
    let mut parts: Vec<String> = Vec::new();
    for a in addr.iter() {
        let email = a.address().unwrap_or_default();
        let name = a.name().unwrap_or_default();
        if !name.is_empty() {
            parts.push(format!("{name} <{email}>"));
        } else if !email.is_empty() {
            parts.push(email.to_string());
        }
    }
    if parts.is_empty() {
        "(unknown sender)".to_string()
    } else {
        parts.join(", ")
    }
}

/// Namespace UUID for everything this provider emits. Generated once
/// (uuidv5(NIL, "frankweiler.jmap")) and frozen forever; any change here
/// invalidates every primary key.
pub const JMAP_NS: Uuid = Uuid::from_bytes([
    0xa3, 0x7d, 0xb1, 0x4f, 0x52, 0x6f, 0x4c, 0xb9, 0x9d, 0x4a, 0x88, 0x3d, 0x1b, 0x42, 0xe5, 0x10,
]);

pub fn thread_uuid(account_id: &str, thread_id: &str) -> String {
    Uuid::new_v5(
        &JMAP_NS,
        format!("jmap:{account_id}:thread:{thread_id}").as_bytes(),
    )
    .to_string()
}

pub fn email_uuid(account_id: &str, email_id: &str) -> String {
    Uuid::new_v5(
        &JMAP_NS,
        format!("jmap:{account_id}:email:{email_id}").as_bytes(),
    )
    .to_string()
}

/// Render every thread in `parsed` under `<root>/rendered_md/jmap/...`,
/// calling `on_doc_complete` per rendered markdown so the orchestrator
/// can commit each `RenderedMarkdown` to the index atomically.
///
/// Skip semantics: the parse layer ran the `dolt_diff_<table>` union
/// query and only loaded buckets for threads that had any touched row
/// since the prior render cursor. `parsed.docs` holds only those
/// threads; everything else is reported via `parsed.docs_skipped`. So
/// this function just iterates and writes — no inline skip check, no
/// `source_fingerprint` compare.
///
/// On a successful render we stamp the new doltlite HEAD into a
/// per-source cursor JSON file at the root of this provider's render
/// dir; the next run will pass that hash back to `parse` as its
/// `from_ref` for the dolt_diff scan.
pub fn render_all(
    parsed: &ParsedEmail,
    root: &Path,
    source_name: &str,
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<Vec<PathBuf>> {
    let elapsed_ms = parsed.scan.scan_elapsed.map(|d| d.as_millis() as u64);
    tracing::info!(
        source = source_name,
        scan_elapsed_ms = elapsed_ms,
        changed_threads = parsed
            .scan
            .changed_threads
            .as_ref()
            .map(|s| s.len() as i64)
            .unwrap_or(-1),
        cold_start = parsed.scan.changed_threads.is_none(),
        "[translate] email dolt_diff scan"
    );
    // Build lookups.
    let mailbox_name: HashMap<String, String> = parsed
        .mailboxes
        .iter()
        .filter_map(|m| {
            let id = m.get("id")?.as_str()?.to_string();
            let name = m
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            Some((id, name))
        })
        .collect();

    let account_slug: HashMap<String, String> = parsed
        .accounts
        .iter()
        .filter_map(|a| {
            let id = a.get("id").and_then(|v| v.as_str())?.to_string();
            let name = a
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(&id)
                .to_string();
            Some((id.clone(), slug_acct(&name, &id)))
        })
        .collect();

    progress.set_length(Some(parsed.docs.len() as u64));
    let mut written: Vec<PathBuf> = Vec::new();

    for bucket in &parsed.docs {
        if bucket.emails.is_empty() {
            progress.inc(1);
            continue;
        }
        let thread_id = &bucket.thread_id;
        let emails: Vec<&LoadedEmail> = bucket.emails.iter().collect();
        let acct = &bucket.account_id;
        let acct_slug = account_slug
            .get(acct)
            .cloned()
            .unwrap_or_else(|| slug_acct(acct, acct));
        let tuid = thread_uuid(acct, thread_id);
        let rel = thread_relative_path(&acct_slug, &tuid);
        let abs = root.join(&rel);
        // The per-doc `source_fingerprint` used to be a bucket-
        // fingerprint hash from a SQL CTE. With dolt_diff driving
        // skip, that compare is gone; use the thread_uuid so the
        // sidecar still has a stable identifier. The orchestrator's
        // prior_fingerprints map is ignored by email now.
        let fp = tuid.clone();

        let page_dir = abs
            .parent()
            .expect("thread relative_path always has a page-dir parent");
        fs::create_dir_all(page_dir)
            .with_context(|| format!("create thread dir {}", page_dir.display()))?;

        // mail-parse each email's `.eml` from the per-bucket bundle
        // once and cache the envelope/body extracts. All sync — the
        // bundle is pre-loaded.
        let mut parsed_emls: HashMap<String, ParsedEml> = HashMap::new();
        for em in &emails {
            let parsed_eml = match bucket.blobs.get(&em.blob_id) {
                Some(blob) => ParsedEml::from_eml_bytes(&blob.bytes),
                None => ParsedEml::default(),
            };
            parsed_emls.insert(em.id.clone(), parsed_eml);
        }

        // Materialize attachments referenced by any email in this
        // thread. Filenames come from `Blob::rendered_filename` (hash +
        // ext) so collisions across attachments are impossible.
        let blobs_dir = page_dir.join("blobs");
        let mut materialized: HashMap<String, String> = HashMap::new();
        for em in &emails {
            if let Some(atts) = bucket.joins.attachments.get(&em.id) {
                for a in atts {
                    if materialized.contains_key(&a.blob_id) {
                        continue;
                    }
                    let Some(blob) = bucket.blobs.get(&a.blob_id) else {
                        continue;
                    };
                    fs::create_dir_all(&blobs_dir)
                        .with_context(|| format!("create blobs dir {}", blobs_dir.display()))?;
                    let fname = blob.rendered_filename();
                    fs::write(blobs_dir.join(&fname), &blob.bytes).with_context(|| {
                        format!("write attachment {}", blobs_dir.join(&fname).display())
                    })?;
                    materialized.insert(a.blob_id.clone(), fname);
                }
            }
        }

        // Materialize inline-image-style parts that mail-parser found in
        // the .eml but the JMAP server excluded from `attachments`
        // (Fastmail, iCloud, …). Filenames are content-hash + extension
        // so two messages sharing the same logo collapse to one file.
        // Two output maps so render_thread_md can route the two cases:
        //   * `inline_cid_to_fname`: cid-tagged parts → fed into the
        //     `<img src="cid:…">` rewrite so they render inline.
        //   * `loose_inline_per_email`: cid-less parts (Apple Mail
        //     iPhone style) → appended after the rendered body as
        //     trailing `![](blobs/…)` links so the image still shows.
        let mut inline_cid_to_fname: HashMap<String, String> = HashMap::new();
        let mut loose_inline_per_email: HashMap<String, Vec<String>> = HashMap::new();
        let mut wrote_blobs_dir = blobs_dir.exists();
        for em in &emails {
            let Some(parsed_eml) = parsed_emls.get(&em.id) else {
                continue;
            };
            for inline in &parsed_eml.inline_parts {
                let fname = inline_blob_filename(&inline.bytes, inline.content_type.as_deref());
                // Lazily create the blobs dir — many emails have no
                // inline parts and we don't want to leave empty dirs.
                if !wrote_blobs_dir {
                    fs::create_dir_all(&blobs_dir)
                        .with_context(|| format!("create blobs dir {}", blobs_dir.display()))?;
                    wrote_blobs_dir = true;
                }
                let path = blobs_dir.join(&fname);
                if !path.exists() {
                    fs::write(&path, &inline.bytes)
                        .with_context(|| format!("write inline part {}", path.display()))?;
                }
                match &inline.cid {
                    Some(cid) => {
                        inline_cid_to_fname.entry(cid.clone()).or_insert(fname);
                    }
                    None => {
                        let bucket = loose_inline_per_email.entry(em.id.clone()).or_default();
                        if !bucket.iter().any(|f| f == &fname) {
                            bucket.push(fname);
                        }
                    }
                }
            }
        }

        // Order: blobs → md → sidecar → callback. Callback is the
        // commit point — interrupted runs leave the indexer
        // un-notified so the next run re-renders.
        let body = render_thread_md(
            &tuid,
            thread_id,
            acct,
            &emails,
            &mailbox_name,
            &bucket.joins,
            &materialized,
            &inline_cid_to_fname,
            &loose_inline_per_email,
            &parsed_emls,
        );
        fs::write(&abs, &body).with_context(|| format!("write {}", abs.display()))?;

        let rows = build_grid_rows(
            thread_id,
            acct,
            &acct_slug,
            &emails,
            &bucket.joins,
            &mailbox_name,
            &parsed_emls,
        );
        let sidecar_path = abs.with_extension("grid_rows.json");
        emit_sidecar(&sidecar_path, &tuid, &fp, RENDER_VERSION, &rows, &[])?;

        on_doc_complete(RenderedMarkdown {
            markdown_uuid: tuid.clone(),
            source_name: source_name.to_string(),
            source_fingerprint: fp,
            upstream_cursor: None,
            md_path: abs.clone(),
            render_version: RENDER_VERSION,
            rows,
            edges: Vec::new(),
        })?;

        written.push(rel);
        progress.inc(1);
    }

    // Advance the cursor only when we got through the whole loop
    // without erroring AND we managed to read HEAD at scan time. A
    // missing HEAD (non-doltlite sqlite) leaves the cursor unwritten;
    // next run is another cold start.
    if let Some(head) = parsed.scan.new_head.as_deref() {
        let cursor_path = render_cursor::cursor_path(root, "email", source_name);
        render_cursor::write(&cursor_path, head, parsed.scan.scan_elapsed)
            .with_context(|| format!("write email render cursor {}", cursor_path.display()))?;
    }
    Ok(written)
}

/// Relative path to a thread's rendered markdown, mirrored from the
/// path the renderer writes to so callers (orchestrator skip logic)
/// can predict it without paying for a render.
fn thread_relative_path(account_slug: &str, thread_uuid: &str) -> PathBuf {
    PathBuf::from("rendered_md/jmap")
        .join(account_slug)
        .join(thread_uuid)
        .join("index.md")
}

// ─────────────────────────────────────────────────────────────────────
// Markdown rendering
// ─────────────────────────────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
fn render_thread_md(
    tuid: &str,
    thread_id: &str,
    account_id: &str,
    emails: &[&LoadedEmail],
    mailbox_name: &HashMap<String, String>,
    joins: &crate::extract::db::EmailJoins,
    materialized: &HashMap<String, String>,
    inline_cid_to_fname: &HashMap<String, String>,
    loose_inline_per_email: &HashMap<String, Vec<String>>,
    parsed_emls: &HashMap<String, ParsedEml>,
) -> String {
    let root = emails.first().unwrap();
    let subject = root.subject.as_deref().unwrap_or("(no subject)");
    let participants = collect_participants(emails, parsed_emls);
    let labels: Vec<String> = emails
        .iter()
        .flat_map(|e| {
            joins
                .mailboxes
                .get(&e.id)
                .map(|v| v.as_slice())
                .unwrap_or(&[])
        })
        .map(|mid| {
            mailbox_name
                .get(mid)
                .cloned()
                .unwrap_or_else(|| mid.clone())
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();
    let keywords: Vec<String> = emails
        .iter()
        .flat_map(|e| {
            joins
                .keywords
                .get(&e.id)
                .map(|v| v.as_slice())
                .unwrap_or(&[])
        })
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect();

    let mut out = String::new();
    out.push_str("---\n");
    out.push_str(&format!("subject: {}\n", yaml_str(subject)));
    out.push_str(&format!("thread_id: {}\n", yaml_str(thread_id)));
    out.push_str(&format!("account_id: {}\n", yaml_str(account_id)));
    out.push_str(&format!("email_count: {}\n", emails.len()));
    out.push_str(&format!(
        "received_at_first: {}\n",
        yaml_str(
            emails
                .first()
                .and_then(|e| e.received_at.as_deref())
                .unwrap_or("")
        ),
    ));
    out.push_str(&format!(
        "received_at_last: {}\n",
        yaml_str(
            emails
                .last()
                .and_then(|e| e.received_at.as_deref())
                .unwrap_or("")
        ),
    ));
    out.push_str("participants:\n");
    for p in &participants {
        out.push_str(&format!("  - {}\n", yaml_str(p)));
    }
    out.push_str("labels:\n");
    for l in &labels {
        out.push_str(&format!("  - {}\n", yaml_str(l)));
    }
    out.push_str("keywords:\n");
    for k in &keywords {
        out.push_str(&format!("  - {}\n", yaml_str(k)));
    }
    out.push_str("---\n\n");
    // Shared `Title` so thread pages carry the same
    // `data-page-title-uuid` hook (copy-page-id button) as every
    // other provider. JMAP doesn't carry a stable web URL per thread,
    // so `source_url` is `None`. (For Fastmail-sourced threads the
    // canonical URL is `https://app.fastmail.com/mail/<mailbox>/
    // <emailId>.<threadId>?u=…` but the trailing `?u=…` account id
    // isn't in our extract data, so wiring it up is a follow-up.)
    out.push_str(
        &frankweiler_etl::title::Title {
            text: subject,
            markdown_uuid: Some(tuid),
            source_url: None,
        }
        .render(),
    );

    for (idx, em) in emails.iter().enumerate() {
        let default_parsed = ParsedEml::default();
        let parsed_eml = parsed_emls.get(&em.id).unwrap_or(&default_parsed);
        let from = if parsed_eml.from_display.is_empty() {
            "(unknown sender)".to_string()
        } else {
            parsed_eml.from_display.clone()
        };
        let when = em.received_at.as_deref().unwrap_or("(unknown date)");
        out.push_str(&format!("## #{} — {} — {}\n\n", idx + 1, from, when));
        let atts = joins
            .attachments
            .get(&em.id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if let Some(body) = email_body_markdown(parsed_eml, atts, materialized, inline_cid_to_fname)
        {
            out.push_str(&body);
            if !body.ends_with('\n') {
                out.push('\n');
            }
        } else {
            let preview = parsed_eml.preview();
            if !preview.is_empty() {
                out.push_str(&preview);
                out.push('\n');
            }
        }

        // Append loose inline parts: cid-less inline parts from the
        // .eml MIME tree (Apple Mail iPhone "drop the image into the
        // message" pattern). The body never references them, so we
        // render each as a trailing `![](blobs/<fname>)`. Skip any
        // filename already mentioned in the body so we don't
        // double-render parts that htmd happened to pick up via some
        // other path.
        if let Some(loose) = loose_inline_per_email.get(&em.id) {
            for fname in loose {
                let link = format!("blobs/{fname}");
                if out.contains(&link) {
                    continue;
                }
                out.push('\n');
                out.push_str(&format!("![](blobs/{fname})\n"));
            }
        }

        // Attachment list — exclude inline attachments that were
        // embedded into the body via `<img src="cid:…">`. They've
        // already been rendered inline; listing them again at the
        // bottom is just noise.
        let trailing: Vec<&LoadedAttachment> =
            atts.iter().filter(|a| !is_inline_attachment(a)).collect();
        if !trailing.is_empty() {
            out.push_str("\n### Attachments\n\n");
            for a in trailing {
                let label = a.name.clone().unwrap_or_else(|| a.part_id.clone());
                if let Some(fname) = materialized.get(&a.blob_id) {
                    out.push_str(&format!("- [{label}](blobs/{fname})\n"));
                } else {
                    out.push_str(&format!(
                        "- {label} _(blob {} not materialized)_\n",
                        a.blob_id
                    ));
                }
            }
        }
        out.push('\n');
    }

    out
}

/// True if this attachment is an inline body part (`disposition ==
/// "inline"` or carries a `Content-ID`), i.e. something the HTML
/// body references via `<img src="cid:…">` rather than an
/// independent file attachment.
fn is_inline_attachment(a: &LoadedAttachment) -> bool {
    a.disposition.as_deref() == Some("inline") || a.cid.is_some()
}

/// Raw concatenated text for the grid_rows `text` column. Uses the
/// `text/plain` part when present (best for search; no HTML noise)
/// and falls back to `text/html` when there's no plain alternative.
fn email_body_plain(parsed: &ParsedEml) -> Option<String> {
    if !parsed.text_body.trim().is_empty() {
        Some(parsed.text_body.clone())
    } else if !parsed.html_body.trim().is_empty() {
        Some(parsed.html_body.clone())
    } else {
        None
    }
}

/// Render one email's body to markdown. Prefers the `text/html` part
/// (run through htmd after rewriting `cid:` srcs to point at
/// materialized blobs) so we get auto-linked URLs, inline images,
/// lists, and blockquotes for free. Falls back to `text/plain` with
/// a light URL-autolink pass when no HTML body is present.
fn email_body_markdown(
    parsed: &ParsedEml,
    attachments: &[LoadedAttachment],
    materialized: &HashMap<String, String>,
    inline_cid_to_fname: &HashMap<String, String>,
) -> Option<String> {
    // Build a cid → "blobs/<filename>" lookup so we can rewrite
    // `<img src="cid:…">` URLs before htmd sees them. JMAP-side cids
    // (from `email_attachments.cid`) win when both sources have an
    // entry for the same id — the JMAP rendered filename comes from
    // the canonical blob in the CAS.
    let mut cid_to_blob: HashMap<String, String> = HashMap::new();
    for (cid, fname) in inline_cid_to_fname {
        cid_to_blob.insert(cid.clone(), format!("blobs/{fname}"));
    }
    for a in attachments {
        let (Some(cid), Some(fname)) = (a.cid.as_deref(), materialized.get(&a.blob_id)) else {
            continue;
        };
        cid_to_blob.insert(cid.to_string(), format!("blobs/{fname}"));
    }

    // Try HTML body first.
    if !parsed.html_body.trim().is_empty() {
        let rewritten = rewrite_cid_srcs(&parsed.html_body, &cid_to_blob);
        let md = htmd::HtmlToMarkdown::builder()
            .skip_tags(vec!["script", "style", "head"])
            .build()
            .convert(&rewritten)
            .unwrap_or_default();
        if !md.trim().is_empty() {
            return Some(md);
        }
    }

    // Plaintext fallback. Autolink bare URLs so they render as
    // clickable links.
    if parsed.text_body.trim().is_empty() {
        return None;
    }
    Some(autolink_bare_urls(&parsed.text_body))
}

/// Replace `src="cid:<id>"` / `src='cid:<id>'` in raw HTML with the
/// path of the materialized blob. Case-insensitive on the `cid:`
/// scheme; preserves the surrounding HTML byte-for-byte.
fn rewrite_cid_srcs(html: &str, cid_to_blob: &HashMap<String, String>) -> String {
    if cid_to_blob.is_empty() {
        return html.to_string();
    }
    // Lowercase once up front so per-iteration `find` is O(remaining)
    // not O(remaining²). Lowercasing preserves byte offsets (ASCII
    // 'A'..='Z' → 'a'..='z'), so positions found in `lower` index
    // straight into `html`.
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    while i < html.len() {
        let Some(rel) = lower[i..].find("cid:") else {
            out.push_str(&html[i..]);
            break;
        };
        let pos = i + rel;
        // Need at least one char before `cid:` and it must be a quote.
        let prev_char = html[..pos].chars().last();
        if !matches!(prev_char, Some('"') | Some('\'')) {
            out.push_str(&html[i..pos + 4]);
            i = pos + 4;
            continue;
        }
        let quote = prev_char.unwrap();
        let after = &html[pos + 4..];
        let Some(end_rel) = after.find(quote) else {
            out.push_str(&html[i..]);
            break;
        };
        let cid = &after[..end_rel];
        out.push_str(&html[i..pos]);
        if let Some(path) = cid_to_blob.get(cid) {
            out.push_str(path);
        } else {
            // Leave the original `cid:<id>` so a reader can see the
            // unresolved reference rather than a silent dead link.
            out.push_str("cid:");
            out.push_str(cid);
        }
        i = pos + 4 + end_rel;
    }
    out
}

/// Minimal bare-URL autolinker for the plaintext-fallback path.
/// Wraps `http://` / `https://` runs in `<…>` (markdown autolink
/// syntax). Stops at whitespace or common terminator chars so we
/// don't swallow trailing punctuation.
fn autolink_bare_urls(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let rest = &s[i..];
        let Some(pos) = rest.find("http://").or_else(|| rest.find("https://")) else {
            out.push_str(rest);
            break;
        };
        out.push_str(&rest[..pos]);
        let after = &rest[pos..];
        let end = after
            .find(|c: char| c.is_whitespace() || matches!(c, '<' | '>' | '"' | '\''))
            .unwrap_or(after.len());
        let mut url = &after[..end];
        // Strip trailing punctuation that's almost never part of the URL.
        while let Some(last) = url.chars().last() {
            if matches!(last, '.' | ',' | ')' | ']' | '!' | '?' | ';' | ':') {
                url = &url[..url.len() - last.len_utf8()];
            } else {
                break;
            }
        }
        if url.is_empty() {
            out.push_str(&after[..end]);
        } else {
            out.push('<');
            out.push_str(url);
            out.push('>');
            out.push_str(&after[url.len()..end]);
        }
        i += pos + end;
    }
    out
}

fn collect_participants(
    emails: &[&LoadedEmail],
    parsed_emls: &HashMap<String, ParsedEml>,
) -> Vec<String> {
    let mut set = std::collections::BTreeSet::new();
    for em in emails {
        if let Some(parsed) = parsed_emls.get(&em.id) {
            for p in &parsed.participants {
                set.insert(p.clone());
            }
        }
    }
    set.into_iter().collect()
}

fn yaml_str(s: &str) -> String {
    // Quote-and-escape strategy that's safe for arbitrary subject lines:
    // double-quote and escape backslash + double-quote. Newlines get
    // turned into spaces so the value stays on one line.
    let cleaned: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    format!("\"{}\"", cleaned.replace('\\', "\\\\").replace('"', "\\\""))
}

// ─────────────────────────────────────────────────────────────────────
// grid_rows sidecar
// ─────────────────────────────────────────────────────────────────────

fn build_grid_rows(
    thread_id: &str,
    account_id: &str,
    account_slug: &str,
    emails: &[&LoadedEmail],
    joins: &crate::extract::db::EmailJoins,
    mailbox_name: &HashMap<String, String>,
    parsed_emls: &HashMap<String, ParsedEml>,
) -> Vec<GridRow> {
    let tuid = thread_uuid(account_id, thread_id);
    let qmd_path = format!("rendered_md/jmap/{account_slug}/{tuid}/index.md");
    let default_parsed = ParsedEml::default();
    let root = emails.first().unwrap();
    let subject = root.subject.clone().unwrap_or_default();
    let preview: String = emails
        .iter()
        .map(|e| parsed_emls.get(&e.id).unwrap_or(&default_parsed).preview())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" / ");

    let label_str = emails
        .iter()
        .flat_map(|e| {
            joins
                .mailboxes
                .get(&e.id)
                .map(|v| v.as_slice())
                .unwrap_or(&[])
        })
        .map(|mid| {
            mailbox_name
                .get(mid)
                .cloned()
                .unwrap_or_else(|| mid.clone())
        })
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect::<Vec<_>>()
        .join(", ");

    let root_parsed = parsed_emls.get(&root.id).unwrap_or(&default_parsed);
    let mut rows = Vec::with_capacity(emails.len() + 1);
    rows.push(GridRow {
        uuid: tuid.clone(),
        provider: "jmap".to_string(),
        kind: "Email Thread".to_string(),
        source_label: "Mail".to_string(),
        when_ts: root.received_at.clone(),
        author: Some(if root_parsed.from_display.is_empty() {
            "(unknown sender)".to_string()
        } else {
            root_parsed.from_display.clone()
        }),
        account: Some(account_id.to_string()),
        project: None,
        org_uuid: None,
        org_name: None,
        channel: if label_str.is_empty() {
            None
        } else {
            Some(label_str)
        },
        conversation_name: Some(subject.clone()),
        conversation_uuid: tuid.clone(),
        message_index: None,
        entire_chat: format!("/chat/{tuid}"),
        text: if preview.is_empty() {
            subject.clone()
        } else {
            format!("{subject}\n\n{preview}")
        },
        slack_link: None,
        qmd_path: Some(qmd_path.clone()),
        source_url: None,
        git_sha: None,
        external_id: Some(thread_id.to_string()),
        notion_page_uuid: None,
        notion_block_uuid: None,
        markdown_uuid: Some(tuid.clone()),
    });

    for (idx, em) in emails.iter().enumerate() {
        let euid = email_uuid(&em.account_id, &em.id);
        let parsed_eml = parsed_emls.get(&em.id).unwrap_or(&default_parsed);
        let body = email_body_plain(parsed_eml).unwrap_or_else(|| parsed_eml.preview());
        rows.push(GridRow {
            uuid: euid,
            provider: "jmap".to_string(),
            kind: "Email".to_string(),
            source_label: "Mail".to_string(),
            when_ts: em.received_at.clone(),
            author: Some(if parsed_eml.from_display.is_empty() {
                "(unknown sender)".to_string()
            } else {
                parsed_eml.from_display.clone()
            }),
            account: Some(account_id.to_string()),
            project: None,
            org_uuid: None,
            org_name: None,
            channel: None,
            conversation_name: Some(subject.clone()),
            conversation_uuid: tuid.clone(),
            message_index: Some(idx as i64),
            entire_chat: format!("/chat/{tuid}"),
            text: format!("{}\n\n{}", em.subject.clone().unwrap_or_default(), body),
            slack_link: None,
            qmd_path: Some(qmd_path.clone()),
            source_url: None,
            git_sha: None,
            external_id: Some(em.id.clone()),
            notion_page_uuid: None,
            notion_block_uuid: None,
            markdown_uuid: Some(tuid.clone()),
        });
    }

    rows
}

// ─────────────────────────────────────────────────────────────────────
// Filename safety + dedup
// ─────────────────────────────────────────────────────────────────────

fn slug_acct(name: &str, fallback: &str) -> String {
    let base = if name.is_empty() { fallback } else { name };
    let s: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    let trimmed = s.trim_matches('_');
    if trimmed.is_empty() {
        "account".to_string()
    } else {
        trimmed.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_acct_handles_emails() {
        assert_eq!(slug_acct("thad@fastmail.com", "A1"), "thad_fastmail_com");
        assert_eq!(slug_acct("", "A1"), "a1");
    }

    #[test]
    fn thread_uuid_is_stable() {
        let a = thread_uuid("A1", "T1");
        let b = thread_uuid("A1", "T1");
        assert_eq!(a, b);
        assert_ne!(a, thread_uuid("A1", "T2"));
    }
}
