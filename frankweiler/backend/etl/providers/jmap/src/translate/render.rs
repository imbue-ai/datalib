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
    Uuid::new_v5(&JMAP_NS, format!("jmap:{account_id}:thread:{thread_id}").as_bytes()).to_string()
}

pub fn email_uuid(account_id: &str, email_id: &str) -> String {
    Uuid::new_v5(&JMAP_NS, format!("jmap:{account_id}:email:{email_id}").as_bytes()).to_string()
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
        if prior_fingerprints.get(&tuid).map(String::as_str) == Some(fp.as_str())
            && abs.exists()
        {
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
        let blobs_dir = page_dir.join("blobs");
        let mut materialized: HashMap<String, String> = HashMap::new();
        for em in emails {
            if let Some(atts) = parsed.joins.attachments.get(&em.id) {
                for a in atts {
                    if materialized.contains_key(&a.blob_id) {
                        continue;
                    }
                    let Some(bytes) = parsed.blobs.read_by_id(&a.blob_id)? else {
                        continue;
                    };
                    fs::create_dir_all(&blobs_dir).with_context(|| {
                        format!("create blobs dir {}", blobs_dir.display())
                    })?;
                    let fname = unique_safe_filename(a, &materialized);
                    fs::write(blobs_dir.join(&fname), &bytes.bytes).with_context(|| {
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
        fs::write(&abs, &body)
            .with_context(|| format!("write {}", abs.display()))?;

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
        };
        let sidecar_path = abs.with_extension("grid_rows.json");
        let sidecar_json =
            serde_json::to_string_pretty(&sidecar).context("serialize sidecar")?;
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
        .map(|mid| mailbox_name.get(mid).cloned().unwrap_or_else(|| mid.clone()))
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
        yaml_str(emails.first().and_then(|e| e.received_at.as_deref()).unwrap_or("")),
    ));
    out.push_str(&format!(
        "received_at_last: {}\n",
        yaml_str(emails.last().and_then(|e| e.received_at.as_deref()).unwrap_or("")),
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
        if let Some(body) = email_body_text(em) {
            out.push_str(&body);
            if !body.ends_with('\n') {
                out.push('\n');
            }
        } else if let Some(preview) = em.payload.get("preview").and_then(|v| v.as_str()) {
            out.push_str(preview);
            out.push('\n');
        }

        if let Some(atts) = joins.attachments.get(&em.id) {
            if !atts.is_empty() {
                out.push_str("\n### Attachments\n\n");
                for a in atts {
                    let label = a.name.clone().unwrap_or_else(|| a.part_id.clone());
                    if let Some(fname) = materialized.get(&a.blob_id) {
                        out.push_str(&format!("- [{label}](blobs/{fname})\n"));
                    } else {
                        out.push_str(&format!("- {label} _(blob {} not materialized)_\n", a.blob_id));
                    }
                }
            }
        }
        out.push('\n');
    }

    out
}

fn email_body_text(em: &LoadedEmail) -> Option<String> {
    let body_values = em.payload.get("bodyValues")?.as_object()?;
    let parts = em
        .payload
        .get("textBody")
        .and_then(|v| v.as_array())
        .or_else(|| em.payload.get("htmlBody").and_then(|v| v.as_array()))?;
    let mut out = String::new();
    for p in parts {
        let Some(part_id) = p.get("partId").and_then(|v| v.as_str()) else {
            continue;
        };
        let Some(bv) = body_values.get(part_id) else {
            continue;
        };
        if let Some(s) = bv.get("value").and_then(|v| v.as_str()) {
            out.push_str(s);
            out.push('\n');
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
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
    format!(
        "\"{}\"",
        cleaned.replace('\\', "\\\\").replace('"', "\\\"")
    )
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
        .flat_map(|e| joins.mailboxes.get(&e.id).map(|v| v.as_slice()).unwrap_or(&[]))
        .map(|mid| mailbox_name.get(mid).cloned().unwrap_or_else(|| mid.clone()))
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
        channel: if label_str.is_empty() { None } else { Some(label_str) },
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
        let body = email_body_text(em).unwrap_or_else(|| {
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
            text: format!(
                "{}\n\n{}",
                em.subject.clone().unwrap_or_default(),
                body
            ),
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

fn unique_safe_filename(a: &LoadedAttachment, taken: &HashMap<String, String>) -> String {
    let base = a.name.clone().unwrap_or_else(|| format!("part-{}", a.part_id));
    let safe = sanitize_filename(&base);
    let used: std::collections::HashSet<&str> = taken.values().map(String::as_str).collect();
    if !used.contains(safe.as_str()) {
        return safe;
    }
    // Collide: append blob_id digest until unique.
    let mut h = Sha256::new();
    h.update(a.blob_id.as_bytes());
    let suffix = format!("{:.8x}", h.finalize());
    let (stem, ext) = split_ext(&safe);
    let with_suffix = if ext.is_empty() {
        format!("{stem}-{suffix}")
    } else {
        format!("{stem}-{suffix}.{ext}")
    };
    with_suffix
}

fn sanitize_filename(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '\0' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    if cleaned.trim().is_empty() {
        return "attachment".to_string();
    }
    if cleaned.len() > 200 {
        let (stem, ext) = split_ext(&cleaned);
        let trimmed_stem: String = stem.chars().take(180).collect();
        if ext.is_empty() {
            trimmed_stem
        } else {
            format!("{trimmed_stem}.{ext}")
        }
    } else {
        cleaned
    }
}

fn split_ext(name: &str) -> (String, String) {
    if let Some(idx) = name.rfind('.') {
        if idx > 0 && idx < name.len() - 1 {
            return (name[..idx].to_string(), name[idx + 1..].to_string());
        }
    }
    (name.to_string(), String::new())
}

fn slug_acct(name: &str, fallback: &str) -> String {
    let base = if name.is_empty() { fallback } else { name };
    let s: String = base
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '_' })
        .collect();
    let trimmed = s.trim_matches('_');
    if trimmed.is_empty() {
        "account".to_string()
    } else {
        trimmed.to_string()
    }
}

fn source_fingerprint(
    emails: &[&LoadedEmail],
    joins: &crate::extract::db::EmailJoins,
) -> String {
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
    fn sanitize_strips_slashes() {
        assert_eq!(sanitize_filename("../etc/passwd"), ".._etc_passwd");
        assert_eq!(sanitize_filename(""), "attachment");
    }

    #[test]
    fn thread_uuid_is_stable() {
        let a = thread_uuid("A1", "T1");
        let b = thread_uuid("A1", "T1");
        assert_eq!(a, b);
        assert_ne!(a, thread_uuid("A1", "T2"));
    }
}
