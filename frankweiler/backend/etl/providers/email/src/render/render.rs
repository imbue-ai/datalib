//! Email (JMAP) render: convert parsed threads into the shared
//! `chat-common` normalized model and delegate markdown / grid-row /
//! sidecar plumbing to [`frankweiler_etl_chat_common::render::render_all`].
//!
//! One thread → one [`NormalizedChat`] (single `"all"` bucket);
//! `chat_uuid`/`markdown_uuid` are the existing `thread_uuid`, so page
//! identities / links stay stable. Each email → one
//! [`NormalizedChatItem`]: the From line is the author, the mailbox
//! labels it's filed under render as a chips line, the body is the
//! mail-parsed `.eml` (HTML → markdown via htmd, `cid:` images
//! rewritten to materialized blobs), and the **quoted reply history is
//! folded into a `<details>`** so each message shows only its new text —
//! a markdown knock-off of Gmail's trimmed-quote view.
//!
//! Attachment + inline-image bytes (the latter extracted from the
//! `.eml` MIME tree) are injected into a per-thread [`BlobBundle`] that
//! chat-common materializes into the page's `blobs/` dir; the raw `.eml`
//! source blobs are deliberately NOT included so they don't litter the
//! output.
//!
//! Incrementality is unchanged and still dolt-diff driven: `parse`
//! narrowed to changed threads, so we pass an empty `prior_fingerprints`
//! map and advance the cursor on success.

use std::collections::{BTreeSet, HashMap, HashSet};

use anyhow::{Context, Result};
use frankweiler_etl::blob_cas::{blake3_hex, BlobBundle};
use frankweiler_etl::grid_index::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl::render_cursor;
use frankweiler_etl_chat_common::render::{render_all as cc_render_all, RenderProfile};
use frankweiler_etl_chat_common::types::{
    ItemKind, NormalizedChat, NormalizedChatItem, NormalizedDoc,
};
use mail_parser::{Address, MessageParser, MimeHeaders, PartType};
use uuid::Uuid;

use super::parse::ParsedEmail;
use crate::download::db::{LoadedAttachment, LoadedEmail};

/// Bump when the item-shape / column mapping changes meaningfully.
/// v3: render via chat-common (+ quoted-text folding, label chips).
pub const RENDER_VERSION: u32 = 3;

/// Which webmail to build each email's `↗` outlink for. Mirrors
/// `frankweiler_core::config::EmailOutlink`; the orchestrator maps the
/// config enum onto this one (the provider crate doesn't depend on core).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutlinkFormat {
    Gmail,
    Fastmail,
}

/// Build the public webmail URL for one email, if the format and the
/// required identifiers are present.
fn email_outlink(
    fmt: Option<OutlinkFormat>,
    em: &LoadedEmail,
    mailbox_labels: &[String],
) -> Option<String> {
    match fmt? {
        OutlinkFormat::Gmail => gmail_outlink(em.message_id.as_deref()?),
        OutlinkFormat::Fastmail => Some(fastmail_outlink(
            primary_mailbox(mailbox_labels),
            &em.id,
            &em.thread_id,
        )),
    }
}

/// `#search/rfc822msgid:` lands on the message from its `Message-ID`
/// alone — robust across Takeout exports where the opaque permalink id
/// isn't available.
fn gmail_outlink(message_id: &str) -> Option<String> {
    let id = message_id
        .trim()
        .trim_start_matches('<')
        .trim_end_matches('>')
        .trim();
    if id.is_empty() {
        return None;
    }
    Some(format!(
        "https://mail.google.com/mail/u/0/#search/rfc822msgid:{}",
        percent_encode(id)
    ))
}

fn fastmail_outlink(mailbox: &str, email_id: &str, thread_id: &str) -> String {
    format!(
        "https://app.fastmail.com/mail/{}/{email_id}.{thread_id}",
        percent_encode(mailbox)
    )
}

/// Pick the mailbox name to put in a Fastmail path: prefer "Inbox", else
/// the first label, else "Inbox".
fn primary_mailbox(labels: &[String]) -> &str {
    labels
        .iter()
        .find(|n| n.eq_ignore_ascii_case("inbox"))
        .or_else(|| labels.first())
        .map(String::as_str)
        .unwrap_or("Inbox")
}

/// Percent-encode everything but RFC 3986 unreserved chars.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn profile() -> RenderProfile {
    RenderProfile {
        provider: "jmap",
        source_label: "Mail".to_string(),
        chat_kind: "Email Thread".to_string(),
        message_kind: "Email".to_string(),
        reaction_kind: "Email Reaction".to_string(),
        render_version: RENDER_VERSION,
    }
}

pub fn render_all(
    parsed: &ParsedEmail,
    root: &std::path::Path,
    source_name: &str,
    outlink: Option<OutlinkFormat>,
    only_labels: &[String],
    progress: &Progress,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<()> {
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
        "[render] email dolt_diff scan"
    );

    // mailbox id → display name (for the per-email label chips).
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

    // Optional render-time label filter. Resolve the configured label
    // paths to mailbox ids against the same tree (`parentId`) the chips
    // use; `None` = render every thread. Resolution and exact-match
    // semantics are shared with the download filter via
    // [`crate::mailbox_labels`], so a given label means the same thing
    // at both phases and across JMAP / mbox sources.
    let label_allow: Option<HashSet<String>> = if only_labels.is_empty() {
        None
    } else {
        let nodes: Vec<crate::mailbox_labels::MailboxNode> = parsed
            .mailboxes
            .iter()
            .filter_map(crate::mailbox_labels::MailboxNode::from_payload)
            .collect();
        let resolved = crate::mailbox_labels::resolve(&nodes, only_labels);
        if !resolved.unmatched.is_empty() {
            tracing::warn!(
                source = source_name,
                unmatched = ?resolved.unmatched,
                "only_render_labels matched no mailbox; check spelling / parent path",
            );
        }
        Some(resolved.ids)
    };

    let mut chats: Vec<NormalizedChat> = Vec::with_capacity(parsed.docs.len());
    let mut blobs_by_chat: HashMap<String, BlobBundle> = HashMap::new();
    for bucket in &parsed.docs {
        if bucket.emails.is_empty() {
            continue;
        }
        // Thread-level inclusion: keep the whole thread if ANY of its
        // emails is filed under an allowed mailbox, so conversations
        // aren't fragmented across the filter boundary.
        if let Some(allow) = &label_allow {
            let in_scope = bucket.emails.iter().any(|em| {
                bucket
                    .joins
                    .mailboxes
                    .get(&em.id)
                    .is_some_and(|ids| ids.iter().any(|m| allow.contains(m)))
            });
            if !in_scope {
                continue;
            }
        }
        let (chat, bundle) = build_chat(bucket, &mailbox_name, outlink);
        blobs_by_chat.insert(chat.id.clone(), bundle);
        chats.push(chat);
    }

    let no_priors: HashMap<String, String> = HashMap::new();
    cc_render_all(
        &profile(),
        &chats,
        root,
        source_name,
        &blobs_by_chat,
        progress,
        &no_priors,
        on_doc_complete,
    )
    .context("email chat-common render")?;

    if let Some(head) = parsed.scan.new_head.as_deref() {
        let cursor_path = render_cursor::cursor_path(root, source_name);
        render_cursor::write(&cursor_path, head, parsed.scan.scan_elapsed)
            .with_context(|| format!("write email render cursor {}", cursor_path.display()))?;
    }
    Ok(())
}

/// One thread → its [`NormalizedChat`] plus the per-thread
/// [`BlobBundle`] of attachment + inline-image bytes for chat-common to
/// materialize.
fn build_chat(
    bucket: &super::parse::EmailThreadBucket,
    mailbox_name: &HashMap<String, String>,
    outlink: Option<OutlinkFormat>,
) -> (NormalizedChat, BlobBundle) {
    let account_id = &bucket.account_id;
    let tuid = thread_uuid(account_id, &bucket.thread_id);
    let subject = bucket
        .emails
        .first()
        .and_then(|e| e.subject.clone())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "(no subject)".to_string());

    // Render-only bundle: attachment + inline bytes, NOT the raw `.eml`
    // source blobs (those would otherwise be written to blobs/ too).
    let mut render_bundle = BlobBundle::new();
    let mut items: Vec<NormalizedChatItem> = Vec::with_capacity(bucket.emails.len());

    for em in &bucket.emails {
        let parsed_eml = bucket
            .blobs
            .get(&em.blob_id)
            .map(|b| ParsedEml::from_eml_bytes(&b.bytes))
            .unwrap_or_default();

        // Inline `.eml` image parts → bundle (content-addressed ref so
        // the same image across replies collapses to one file).
        let mut inline_cid_to_fname: HashMap<String, String> = HashMap::new();
        let mut loose_inline: Vec<String> = Vec::new();
        for inline in &parsed_eml.inline_parts {
            let b3 = blake3_hex(&inline.bytes);
            let ref_id = format!("inline:{b3}");
            render_bundle.add(
                ref_id.clone(),
                inline.bytes.clone(),
                inline.content_type.clone(),
                None,
            );
            let fname = render_bundle
                .get(&ref_id)
                .map(|b| b.rendered_filename())
                .unwrap_or_default();
            match &inline.cid {
                Some(cid) => {
                    inline_cid_to_fname.entry(cid.clone()).or_insert(fname);
                }
                None => {
                    if !loose_inline.iter().any(|f| f == &fname) {
                        loose_inline.push(fname);
                    }
                }
            }
        }

        // Real downloadable attachments → bundle; collect blob_id→fname
        // for the cid rewrite + the "### Attachments" list.
        let atts: &[LoadedAttachment] = bucket
            .joins
            .attachments
            .get(&em.id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        let mut materialized: HashMap<String, String> = HashMap::new();
        for a in atts {
            if let Some(blob) = bucket.blobs.get(&a.blob_id) {
                let fname = blob.rendered_filename();
                render_bundle.add(
                    a.blob_id.clone(),
                    blob.bytes.clone(),
                    blob.content_type.clone(),
                    blob.upstream_name.clone(),
                );
                materialized.insert(a.blob_id.clone(), fname);
            }
        }

        // Body markdown (HTML → md, cid → blobs/<fname>), then fold the
        // quoted reply history.
        let body = email_body_markdown(&parsed_eml, atts, &materialized, &inline_cid_to_fname)
            .unwrap_or_default();
        let (fresh, quoted) = split_quoted(&body);

        let mut text = String::new();
        // Label chips: which mailboxes this email is filed under.
        let labels = labels_for_email(em, bucket, mailbox_name);
        if !labels.is_empty() {
            text.push_str(&format!("🏷 {}\n\n", labels.join(" · ")));
        }
        text.push_str(fresh.trim_end());
        if let Some(q) = quoted {
            text.push_str("\n\n");
            text.push_str(&q);
        }
        // Loose inline parts the body never referenced.
        for fname in &loose_inline {
            let link = format!("blobs/{fname}");
            if !text.contains(&link) {
                text.push_str(&format!("\n\n![](blobs/{fname})"));
            }
        }
        // Attachment list (excluding inline parts already in the body).
        let trailing: Vec<&LoadedAttachment> =
            atts.iter().filter(|a| !is_inline_attachment(a)).collect();
        if !trailing.is_empty() {
            text.push_str("\n\n### Attachments\n");
            for a in trailing {
                let label = a.name.clone().unwrap_or_else(|| a.part_id.clone());
                match materialized.get(&a.blob_id) {
                    Some(fname) => text.push_str(&format!("\n- [{label}](blobs/{fname})")),
                    None => text.push_str(&format!("\n- {label} _(blob not materialized)_")),
                }
            }
        }

        items.push(NormalizedChatItem {
            message_uuid: email_uuid(&em.account_id, &em.id),
            author_id: em.account_id.clone(),
            author_display: if parsed_eml.from_display.is_empty() {
                "(unknown sender)".to_string()
            } else {
                parsed_eml.from_display.clone()
            },
            date_ms: em.received_at.as_deref().and_then(iso_to_ms).unwrap_or(0),
            text: (!text.trim().is_empty()).then_some(text),
            kind: ItemKind::Text,
            attachments: Vec::new(),
            reactions: Vec::new(),
            system_note: None,
            // Per-email `↗` outlink into the source webmail.
            source_url: email_outlink(outlink, em, &labels),
            kind_label: None,
        });
    }

    // The thread's title `↗` points at the root (first) email's outlink.
    let thread_source_url = items.first().and_then(|i| i.source_url.clone());
    let chat = NormalizedChat {
        id: tuid.clone(),
        chat_uuid: tuid.clone(),
        display: subject.clone(),
        title: Some(subject),
        account: Some(account_id.clone()),
        project: None,
        external_id: Some(bucket.thread_id.clone()),
        source_url: thread_source_url,
        org_uuid: None,
        org_name: None,
        buckets: vec![NormalizedDoc {
            period_key: "all".to_string(),
            markdown_uuid: tuid,
            items,
        }],
    };
    (chat, render_bundle)
}

/// Mailbox/label display names this email is filed under, sorted.
fn labels_for_email(
    em: &LoadedEmail,
    bucket: &super::parse::EmailThreadBucket,
    mailbox_name: &HashMap<String, String>,
) -> Vec<String> {
    bucket
        .joins
        .mailboxes
        .get(&em.id)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .map(|mid| {
            mailbox_name
                .get(mid)
                .cloned()
                .unwrap_or_else(|| mid.clone())
        })
        .filter(|s| !s.is_empty())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Parse an ISO-8601 timestamp to unix millis; `None` on anything
/// unparseable.
fn iso_to_ms(s: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.timestamp_millis())
}

// ─────────────────────────────────────────────────────────────────────
// Quoted-text folding (the Gmail-style trimmed-quote view)
// ─────────────────────────────────────────────────────────────────────

/// Split a rendered email body into (fresh, quoted) where `quoted` is
/// the reply history to collapse into a `<details>`. Conservative: only
/// folds when a recognizable quote marker is found, and never when the
/// fresh part would be empty (don't hide the whole message).
///
/// Recognized markers (covering Gmail, Apple Mail, Outlook):
///   * an attribution line — `On <…> wrote:`, `Le <…> a écrit :`
///   * `-----Original Message-----`
///   * the start of a trailing run of `>`-quoted lines
fn split_quoted(body: &str) -> (String, Option<String>) {
    let lines: Vec<&str> = body.lines().collect();
    let mut cut: Option<usize> = None;

    for (i, raw) in lines.iter().enumerate() {
        let l = raw.trim();
        let is_attr = (l.starts_with("On ") && l.ends_with("wrote:"))
            || (l.starts_with("Le ") && l.ends_with("a écrit :"))
            || l.contains("-----Original Message-----")
            || l.starts_with("> On ") && l.ends_with("wrote:");
        if is_attr {
            cut = Some(i);
            break;
        }
    }

    // Fallback: a trailing contiguous run of blockquote lines (>= 2)
    // with nothing but blanks/quotes to the end of the body.
    if cut.is_none() {
        let mut start: Option<usize> = None;
        for (i, raw) in lines.iter().enumerate() {
            let l = raw.trim_start();
            if l.starts_with('>') {
                if start.is_none() {
                    start = Some(i);
                }
            } else if !l.is_empty() {
                start = None; // run broken by real content
            }
        }
        if let Some(s) = start {
            // Require the run to be more than one line to be worth folding.
            let quoted_lines = lines[s..].iter().filter(|l| !l.trim().is_empty()).count();
            if quoted_lines >= 2 {
                cut = Some(s);
            }
        }
    }

    let Some(cut) = cut else {
        return (body.to_string(), None);
    };
    let fresh = lines[..cut].join("\n");
    if fresh.trim().is_empty() {
        // The whole body is quoted — show it rather than hide everything.
        return (body.to_string(), None);
    }
    let quoted_body = lines[cut..].join("\n");
    let summary = lines[cut].trim().trim_start_matches('>').trim();
    let summary = if summary.is_empty() || summary.len() > 120 {
        "Quoted text".to_string()
    } else {
        summary.to_string()
    };
    let details = format!("<details><summary>{summary}</summary>\n\n{quoted_body}\n\n</details>");
    (fresh, Some(details))
}

// ─────────────────────────────────────────────────────────────────────
// .eml parsing (mail-parser) — body, inline parts, addresses.
// ─────────────────────────────────────────────────────────────────────

/// A binary body part embedded in the `.eml` (cid-referenced inline
/// image, or an Apple-Mail-style "loose" inline part).
#[derive(Default, Clone)]
struct InlinePart {
    cid: Option<String>,
    content_type: Option<String>,
    bytes: Vec<u8>,
}

/// Everything the renderer needs from the RFC 5322 `.eml`, extracted in
/// one mail-parser pass.
#[derive(Default)]
struct ParsedEml {
    from_display: String,
    text_body: String,
    html_body: String,
    inline_parts: Vec<InlinePart>,
}

impl ParsedEml {
    fn from_eml_bytes(bytes: &[u8]) -> Self {
        let Some(msg) = MessageParser::default().parse(bytes) else {
            return Self::default();
        };
        let from_display = format_address(msg.from());
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
            let content_type = part.content_type().map(|ct| match ct.subtype() {
                Some(sub) => format!("{}/{}", ct.ctype(), sub),
                None => ct.ctype().to_string(),
            });
            inline_parts.push(InlinePart {
                cid,
                content_type,
                bytes,
            });
        }
        Self {
            from_display,
            text_body,
            html_body,
            inline_parts,
        }
    }
}

fn part_text(part: &mail_parser::MessagePart<'_>) -> String {
    match &part.body {
        PartType::Text(s) | PartType::Html(s) => s.to_string(),
        // Inline image bytes belong in the blob CAS, not lossy-decoded
        // into the body string.
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

/// True if this attachment is an inline body part (`disposition ==
/// "inline"` or carries a `Content-ID`).
fn is_inline_attachment(a: &LoadedAttachment) -> bool {
    a.disposition.as_deref() == Some("inline") || a.cid.is_some()
}

/// Render one email's body to markdown. Prefers the HTML part (htmd
/// after rewriting `cid:` srcs to materialized blobs); falls back to
/// plaintext with a light URL-autolink pass.
fn email_body_markdown(
    parsed: &ParsedEml,
    attachments: &[LoadedAttachment],
    materialized: &HashMap<String, String>,
    inline_cid_to_fname: &HashMap<String, String>,
) -> Option<String> {
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
    if parsed.text_body.trim().is_empty() {
        return None;
    }
    Some(autolink_bare_urls(&parsed.text_body))
}

/// Replace `src="cid:<id>"` in raw HTML with the materialized blob path.
fn rewrite_cid_srcs(html: &str, cid_to_blob: &HashMap<String, String>) -> String {
    if cid_to_blob.is_empty() {
        return html.to_string();
    }
    let lower = html.to_ascii_lowercase();
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    while i < html.len() {
        let Some(rel) = lower[i..].find("cid:") else {
            out.push_str(&html[i..]);
            break;
        };
        let pos = i + rel;
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
            out.push_str("cid:");
            out.push_str(cid);
        }
        i = pos + 4 + end_rel;
    }
    out
}

/// Minimal bare-URL autolinker for the plaintext-fallback path.
fn autolink_bare_urls(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < s.len() {
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

// ─────────────────────────────────────────────────────────────────────
// UUID recipes (stable across the migration).
// ─────────────────────────────────────────────────────────────────────

/// Namespace UUID for everything this provider emits — frozen forever.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thread_uuid_is_stable() {
        assert_eq!(thread_uuid("acct", "t1"), thread_uuid("acct", "t1"));
        assert_ne!(thread_uuid("acct", "t1"), thread_uuid("acct", "t2"));
    }

    #[test]
    fn split_quoted_folds_attribution_and_keeps_fresh() {
        let body = "Sounds good, let's do it.\n\nOn Tue, Apr 14, Data wrote:\n> The warp core is at 99.7%.\n> Proceed?";
        let (fresh, quoted) = split_quoted(body);
        assert_eq!(fresh.trim(), "Sounds good, let's do it.");
        let q = quoted.expect("quoted folded");
        assert!(q.contains("<details>"));
        assert!(q.contains("On Tue, Apr 14, Data wrote:"));
        assert!(q.contains("The warp core is at 99.7%."));
    }

    #[test]
    fn split_quoted_no_marker_leaves_body_whole() {
        let body = "Just a plain message with no quoting.";
        let (fresh, quoted) = split_quoted(body);
        assert_eq!(fresh, body);
        assert!(quoted.is_none());
    }

    #[test]
    fn split_quoted_all_quoted_is_not_hidden() {
        let body = "On Mon, X wrote:\n> everything is quoted";
        let (fresh, quoted) = split_quoted(body);
        assert_eq!(fresh, body);
        assert!(quoted.is_none());
    }

    #[test]
    fn gmail_outlink_uses_rfc822msgid_search() {
        let url = gmail_outlink("<abc.123@mail.example.com>").unwrap();
        assert_eq!(
            url,
            "https://mail.google.com/mail/u/0/#search/rfc822msgid:abc.123%40mail.example.com"
        );
        assert!(gmail_outlink("   ").is_none());
    }

    #[test]
    fn fastmail_outlink_is_mailbox_email_thread() {
        assert_eq!(
            fastmail_outlink("Inbox", "Em1", "Th1"),
            "https://app.fastmail.com/mail/Inbox/Em1.Th1"
        );
        // mailbox names are path-encoded
        assert_eq!(
            fastmail_outlink("Sent Items", "E", "T"),
            "https://app.fastmail.com/mail/Sent%20Items/E.T"
        );
    }

    #[test]
    fn primary_mailbox_prefers_inbox() {
        assert_eq!(
            primary_mailbox(&["Archive".into(), "Inbox".into()]),
            "Inbox"
        );
        assert_eq!(primary_mailbox(&["Work".into()]), "Work");
        assert_eq!(primary_mailbox(&[]), "Inbox");
    }
}
