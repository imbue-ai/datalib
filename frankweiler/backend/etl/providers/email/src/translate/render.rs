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
//! section's body is the JMAP `bodyValues` text representation when
//! available, else the `preview`. Attachment links resolve to the
//! sibling `blobs/` directory; the byte-perfect copy lives in the
//! `blobs` doltlite table.
//!
//! The grid_rows sidecar emits two row kinds: one `"Email Thread"`
//! row for the thread itself + one `"Email"` row per email. Both
//! share `conversation_uuid = <thread_uuid>` so the existing
//! conversation_uuid filter machinery in the grid backend "just works".

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl::sidecar::{Sidecar, SidecarHeader};
use frankweiler_schema::grid_rows::GridRow;
use serde_json::Value;
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::RENDER_VERSION;
use crate::extract::db::{LoadedAttachment, LoadedEmail, LoadedRaw};

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
/// Skip semantics match the rest of the providers: a thread is skipped
/// (without shredding or writing) when `prior_fingerprints[thread_uuid]`
/// equals the current fingerprint AND the `index.md` is still on disk.
/// Returns the list of relative md paths the orchestrator can hand to
/// downstream consumers (matches chatgpt / anthropic / slack shape).
pub fn render_all(
    parsed: &LoadedRaw,
    root: &Path,
    source_name: &str,
    progress: &Progress,
    prior_fingerprints: &HashMap<String, String>,
    on_doc_complete: &mut dyn FnMut(RenderedMarkdown) -> Result<()>,
) -> Result<Vec<PathBuf>> {
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

    // Group emails by thread; sort each thread by receivedAt for a
    // deterministic render.
    let mut by_thread: BTreeMap<String, Vec<&LoadedEmail>> = BTreeMap::new();
    for em in &parsed.emails {
        by_thread.entry(em.thread_id.clone()).or_default().push(em);
    }
    for emails in by_thread.values_mut() {
        emails.sort_by(|a, b| {
            a.received_at
                .as_deref()
                .unwrap_or("")
                .cmp(b.received_at.as_deref().unwrap_or(""))
        });
    }

    progress.set_length(Some(by_thread.len() as u64));
    let mut written: Vec<PathBuf> = Vec::new();

    for (thread_id, emails) in &by_thread {
        if emails.is_empty() {
            progress.inc(1);
            continue;
        }
        let acct = &emails[0].account_id;
        let acct_slug = account_slug
            .get(acct)
            .cloned()
            .unwrap_or_else(|| slug_acct(acct, acct));
        let tuid = thread_uuid(acct, thread_id);
        let rel = thread_relative_path(&acct_slug, &tuid);
        let abs = root.join(&rel);

        let fp = source_fingerprint(emails, &parsed.joins);

        // Skip when the indexer has the same fingerprint AND the md is
        // still on disk. Matches chatgpt / anthropic / slack: a hand-
        // edited `rm -rf rendered_md/` is recoverable on the next run.
        if prior_fingerprints.get(&tuid).map(String::as_str) == Some(fp.as_str()) && abs.exists() {
            written.push(rel);
            progress.inc(1);
            continue;
        }

        let page_dir = abs
            .parent()
            .expect("thread relative_path always has a page-dir parent");
        fs::create_dir_all(page_dir)
            .with_context(|| format!("create thread dir {}", page_dir.display()))?;

        // Materialize attachments referenced by any email in this thread.
        // Filenames come from `BlobView::rendered_filename` (hash + ext)
        // so collisions across attachments are impossible.
        let blobs_dir = page_dir.join("blobs");
        let mut materialized: HashMap<String, String> = HashMap::new();
        for em in emails {
            if let Some(atts) = parsed.joins.attachments.get(&em.id) {
                for a in atts {
                    if materialized.contains_key(&a.blob_id) {
                        continue;
                    }
                    let Some(view) = parsed.blobs.read_by_ref_id(&a.blob_id)? else {
                        continue;
                    };
                    fs::create_dir_all(&blobs_dir)
                        .with_context(|| format!("create blobs dir {}", blobs_dir.display()))?;
                    let fname = view.rendered_filename();
                    fs::write(blobs_dir.join(&fname), &view.bytes).with_context(|| {
                        format!("write attachment {}", blobs_dir.join(&fname).display())
                    })?;
                    materialized.insert(a.blob_id.clone(), fname);
                }
            }
        }

        // Order: blobs → md → sidecar → callback. Callback is the
        // commit point — interrupted runs leave the indexer
        // un-notified so the next run re-renders.
        let body = render_thread_md(
            thread_id,
            acct,
            emails,
            &mailbox_name,
            &parsed.joins,
            &materialized,
        );
        fs::write(&abs, &body).with_context(|| format!("write {}", abs.display()))?;

        let rows = build_grid_rows(
            thread_id,
            acct,
            &acct_slug,
            emails,
            &parsed.joins,
            &mailbox_name,
        );
        let sidecar = Sidecar {
            header: SidecarHeader {
                markdown_uuid: tuid.clone(),
                source_fingerprint: fp.clone(),
                render_version: RENDER_VERSION,
            },
            rows: rows.clone(),
            edges: Vec::new(),
        };
        let sidecar_path = abs.with_extension("grid_rows.json");
        let sidecar_json = serde_json::to_string_pretty(&sidecar).context("serialize sidecar")?;
        fs::write(&sidecar_path, sidecar_json)
            .with_context(|| format!("write {}", sidecar_path.display()))?;

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

fn render_thread_md(
    thread_id: &str,
    account_id: &str,
    emails: &[&LoadedEmail],
    mailbox_name: &HashMap<String, String>,
    joins: &crate::extract::db::EmailJoins,
    materialized: &HashMap<String, String>,
) -> String {
    let root = emails.first().unwrap();
    let subject = root.subject.as_deref().unwrap_or("(no subject)");
    let participants = collect_participants(emails);
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
    out.push_str(&format!("# {}\n\n", subject));

    for (idx, em) in emails.iter().enumerate() {
        let from = format_addresses(em.payload.get("from"));
        let when = em.received_at.as_deref().unwrap_or("(unknown date)");
        out.push_str(&format!("## #{} — {} — {}\n\n", idx + 1, from, when));
        let atts = joins
            .attachments
            .get(&em.id)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        if let Some(body) = email_body_markdown(em, atts, materialized) {
            out.push_str(&body);
            if !body.ends_with('\n') {
                out.push('\n');
            }
        } else if let Some(preview) = em.payload.get("preview").and_then(|v| v.as_str()) {
            out.push_str(preview);
            out.push('\n');
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
/// and falls back to `text/html` stripped to its `bodyValues` text.
/// Different from [`email_body_markdown`] which prefers HTML and
/// runs html2md for the on-disk markdown render.
fn email_body_plain(em: &LoadedEmail) -> Option<String> {
    let body_values = em.payload.get("bodyValues")?.as_object()?;
    let parts = em
        .payload
        .get("textBody")
        .and_then(|v| v.as_array())
        .or_else(|| em.payload.get("htmlBody").and_then(|v| v.as_array()))?;
    let mut out = String::new();
    for p in parts {
        if let Some(part_id) = p.get("partId").and_then(|v| v.as_str()) {
            if let Some(bv) = body_values.get(part_id) {
                if let Some(s) = bv.get("value").and_then(|v| v.as_str()) {
                    out.push_str(s);
                    out.push('\n');
                }
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Render one email's body to markdown. Prefers the `text/html` part
/// (run through html2md after rewriting `cid:` srcs to point at
/// materialized blobs) so we get auto-linked URLs, inline images,
/// lists, and blockquotes for free. Falls back to `text/plain` with
/// a light URL-autolink pass when no HTML body is present.
fn email_body_markdown(
    em: &LoadedEmail,
    attachments: &[LoadedAttachment],
    materialized: &HashMap<String, String>,
) -> Option<String> {
    let body_values = em.payload.get("bodyValues")?.as_object()?;

    // Build a cid → "blobs/<filename>" lookup so we can rewrite
    // `<img src="cid:…">` URLs before html2md sees them. `cid` in
    // JMAP is the bare Message-ID-style token (no `cid:` prefix);
    // the HTML carries the `cid:` URL scheme around it.
    let mut cid_to_blob: HashMap<String, String> = HashMap::new();
    for a in attachments {
        let (Some(cid), Some(fname)) = (a.cid.as_deref(), materialized.get(&a.blob_id)) else {
            continue;
        };
        cid_to_blob.insert(cid.to_string(), format!("blobs/{fname}"));
    }

    // Try HTML body first.
    if let Some(html_parts) = em.payload.get("htmlBody").and_then(|v| v.as_array()) {
        let mut html = String::new();
        for p in html_parts {
            if let Some(part_id) = p.get("partId").and_then(|v| v.as_str()) {
                if let Some(bv) = body_values.get(part_id) {
                    if let Some(s) = bv.get("value").and_then(|v| v.as_str()) {
                        html.push_str(s);
                        html.push('\n');
                    }
                }
            }
        }
        if !html.is_empty() {
            let rewritten = rewrite_cid_srcs(&html, &cid_to_blob);
            let stripped = strip_noisy_blocks(&rewritten);
            let md = html2md::parse_html(&stripped);
            if !md.trim().is_empty() {
                return Some(md);
            }
        }
    }

    // Plaintext fallback. html2md handles bare URLs via the HTML
    // path naturally — for plaintext, the email is unlikely to
    // contain inline images (no MIME structure), but bare URLs
    // are common. Autolink them so they render as clickable links.
    let plain_parts = em.payload.get("textBody")?.as_array()?;
    let mut plain = String::new();
    for p in plain_parts {
        if let Some(part_id) = p.get("partId").and_then(|v| v.as_str()) {
            if let Some(bv) = body_values.get(part_id) {
                if let Some(s) = bv.get("value").and_then(|v| v.as_str()) {
                    plain.push_str(s);
                    plain.push('\n');
                }
            }
        }
    }
    if plain.is_empty() {
        return None;
    }
    Some(autolink_bare_urls(&plain))
}

/// Replace `src="cid:<id>"` / `src='cid:<id>'` in raw HTML with the
/// path of the materialized blob. Case-insensitive on the `cid:`
/// scheme; preserves the surrounding HTML byte-for-byte.
fn rewrite_cid_srcs(html: &str, cid_to_blob: &HashMap<String, String>) -> String {
    if cid_to_blob.is_empty() {
        return html.to_string();
    }
    let mut out = String::with_capacity(html.len());
    let bytes = html.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        // Look for `cid:` (case-insensitive) preceded by `"` or `'`
        // (i.e. the start of an attribute value).
        let rest = &html[i..];
        let lower = rest.to_ascii_lowercase();
        let Some(pos) = lower.find("cid:") else {
            out.push_str(rest);
            break;
        };
        // Need at least one char before `cid:` and it must be a quote.
        if pos == 0 {
            out.push_str(&rest[..pos + 4]);
            i += pos + 4;
            continue;
        }
        let prev_char = rest[..pos].chars().last();
        if !matches!(prev_char, Some('"') | Some('\'')) {
            out.push_str(&rest[..pos + 4]);
            i += pos + 4;
            continue;
        }
        let quote = prev_char.unwrap();
        // Find end-quote.
        let after = &rest[pos + 4..];
        let Some(end_rel) = after.find(quote) else {
            out.push_str(rest);
            break;
        };
        let cid = &after[..end_rel];
        out.push_str(&rest[..pos]);
        if let Some(path) = cid_to_blob.get(cid) {
            out.push_str(path);
        } else {
            // Leave the original `cid:<id>` so a reader can see the
            // unresolved reference rather than a silent dead link.
            out.push_str("cid:");
            out.push_str(cid);
        }
        i += pos + 4 + end_rel;
    }
    out
}

/// Remove `<style>…</style>`, `<script>…</script>`, and `<head>…</head>`
/// blocks (case-insensitive) before html2md sees them. html2md treats
/// CSS / JS as plain text and dumps the whole stylesheet into the
/// rendered output, which is what produces the wall of `:root {…}`
/// noise at the top of marketing emails.
fn strip_noisy_blocks(html: &str) -> String {
    let mut out = html.to_string();
    for tag in ["style", "script", "head"] {
        out = strip_tag_block(&out, tag);
    }
    out
}

fn strip_tag_block(html: &str, tag: &str) -> String {
    let lower = html.to_ascii_lowercase();
    let open_needle = format!("<{tag}");
    let close_needle = format!("</{tag}>");
    let mut out = String::with_capacity(html.len());
    let mut i = 0;
    while i < html.len() {
        let Some(rel_open) = lower[i..].find(&open_needle) else {
            out.push_str(&html[i..]);
            break;
        };
        let open_at = i + rel_open;
        // Confirm the next char after `<tag` is `>` or whitespace —
        // otherwise it's a different tag with the same prefix
        // (e.g. `<header>` when stripping `<head>`).
        let after_name = open_at + open_needle.len();
        let boundary_ok = lower
            .as_bytes()
            .get(after_name)
            .map(|c| matches!(c, b'>' | b' ' | b'\t' | b'\n' | b'\r' | b'/'))
            .unwrap_or(false);
        if !boundary_ok {
            out.push_str(&html[i..after_name]);
            i = after_name;
            continue;
        }
        out.push_str(&html[i..open_at]);
        match lower[after_name..].find(&close_needle) {
            Some(rel_close) => {
                i = after_name + rel_close + close_needle.len();
            }
            None => {
                // Unterminated — drop everything from the open tag to EOF.
                break;
            }
        }
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

fn format_addresses(v: Option<&Value>) -> String {
    let arr = v.and_then(|v| v.as_array());
    let Some(arr) = arr else {
        return "(unknown sender)".to_string();
    };
    let mut parts = Vec::new();
    for a in arr {
        let name = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let email = a.get("email").and_then(|v| v.as_str()).unwrap_or("");
        if !name.is_empty() {
            parts.push(format!("{name} <{email}>"));
        } else {
            parts.push(email.to_string());
        }
    }
    parts.join(", ")
}

fn collect_participants(emails: &[&LoadedEmail]) -> Vec<String> {
    let mut set = std::collections::BTreeSet::new();
    for em in emails {
        for key in ["from", "to", "cc"] {
            if let Some(arr) = em.payload.get(key).and_then(|v| v.as_array()) {
                for a in arr {
                    let email = a.get("email").and_then(|v| v.as_str()).unwrap_or("");
                    if !email.is_empty() {
                        set.insert(email.to_string());
                    }
                }
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
) -> Vec<GridRow> {
    let tuid = thread_uuid(account_id, thread_id);
    let qmd_path = format!("rendered_md/jmap/{account_slug}/{tuid}/index.md");
    let root = emails.first().unwrap();
    let subject = root.subject.clone().unwrap_or_default();
    let preview: String = emails
        .iter()
        .filter_map(|e| e.payload.get("preview").and_then(|v| v.as_str()))
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

    let mut rows = Vec::with_capacity(emails.len() + 1);
    rows.push(GridRow {
        uuid: tuid.clone(),
        provider: "jmap".to_string(),
        kind: "Email Thread".to_string(),
        source_label: "Mail".to_string(),
        when_ts: root.received_at.clone().unwrap_or_default(),
        author: Some(format_addresses(root.payload.get("from"))),
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
        let body = email_body_plain(em).unwrap_or_else(|| {
            em.payload
                .get("preview")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string()
        });
        rows.push(GridRow {
            uuid: euid,
            provider: "jmap".to_string(),
            kind: "Email".to_string(),
            source_label: "Mail".to_string(),
            when_ts: em.received_at.clone().unwrap_or_default(),
            author: Some(format_addresses(em.payload.get("from"))),
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

fn source_fingerprint(emails: &[&LoadedEmail], joins: &crate::extract::db::EmailJoins) -> String {
    let mut h = Sha256::new();
    h.update(RENDER_VERSION.to_le_bytes());
    for em in emails {
        h.update(em.id.as_bytes());
        h.update(b"\n");
        h.update(em.payload.to_string().as_bytes());
        h.update(b"\n");
        if let Some(ms) = joins.mailboxes.get(&em.id) {
            for m in ms {
                h.update(m.as_bytes());
                h.update(b",");
            }
        }
        h.update(b"\n");
        if let Some(ks) = joins.keywords.get(&em.id) {
            for k in ks {
                h.update(k.as_bytes());
                h.update(b",");
            }
        }
        h.update(b"\n");
    }
    format!("{:x}", h.finalize())
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
