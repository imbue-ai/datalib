//! mbox → [`LoadedRaw`]: read an RFC 4155 mbox file (the shape Google
//! Takeout produces: `Mail/All mail Including Spam and Trash.mbox`)
//! and synthesize a JMAP-shaped `LoadedRaw` so the render path runs
//! unchanged.
//!
//! ## Why this lives next to translate, not extract
//!
//! Mirrors the contacts provider's translate-only `.vcf` mode
//! ([`crate::translate::parse`] for vCards): when a source has no
//! `sync:` block in config, we never spin up the doltlite raw db at
//! all — translate reads the file off disk directly and hands a
//! [`LoadedRaw`] to [`crate::translate::render::render_all`]. Lets us
//! check in mbox fixtures (e.g. a Star Trek themed test corpus) and
//! exercise the full email plumbing without a JMAP server in the
//! loop.
//!
//! ## Stable identifiers
//!
//! Re-ingesting the same mbox must produce byte-identical rows. All
//! ids derive from the message contents or its mbox-level location,
//! never from wall-clock time or filesystem-order:
//!
//!   * `account_id` — file stem of the mbox (e.g.
//!     `all_mail_including_spam_and_trash`), or the caller-supplied
//!     override. Lets a checked-in fixture file move between machines
//!     without re-keying.
//!   * `email_id` — the `Message-Id` header verbatim (angle brackets
//!     stripped), falling back to `sha256(raw_eml_bytes)` hex when
//!     the header is missing.
//!   * `thread_id` — `X-GM-THRID` verbatim (Gmail Takeout always
//!     emits it). Falls back to the email's own message-id (a
//!     single-message thread) when absent.
//!   * `mailbox_id` — short hex `sha256("mbox:" + account + ":" +
//!     label_name)`. Two messages tagged `"Inbox"` always land in the
//!     same mailbox row.
//!   * `attachment.partId` — the dotted MIME part path
//!     (`"2"`, `"2.1"`, …) the parser walks. Deterministic from the
//!     message tree.
//!   * `attachment.blobId` and `email.blobId` — `sha256(bytes)` hex.
//!     Content-addressed: re-ingest yields the same blob row.
//!
//! ## Gmail label → JMAP `role` mapping
//!
//! Google Takeout writes a comma-separated `X-Gmail-Labels` header
//! per message. We line them up with JMAP's standard mailbox roles
//! where possible so downstream consumers can filter `role=inbox`
//! identically whether the data came from Fastmail or Google:
//!
//! | Gmail label                  | JMAP mailbox role / keyword |
//! |------------------------------|-----------------------------|
//! | `Inbox`                      | role=`inbox`                |
//! | `Sent`                       | role=`sent`                 |
//! | `Drafts` / `Draft`           | role=`drafts`               |
//! | `Trash`                      | role=`trash`                |
//! | `Spam`                       | role=`junk`                 |
//! | `Archived`                   | (no mailbox — absence)      |
//! | `Unread`                     | (absence of `$seen`)        |
//! | `Opened` / `Read`            | keyword `$seen`             |
//! | `Starred`                    | keyword `$flagged`          |
//! | `Important`                  | keyword `$important`        |
//! | `Category Promotions` (etc.) | role=`null`, name kept      |
//! | (any other user label)       | role=`null`, name kept      |
//!
//! `Archived` and `Unread` are pure absence-states in JMAP — Gmail
//! emits them as positive flags and we translate. `$seen` is the
//! complement of `Unread`: every message gets `$seen` unless
//! `Unread` is present.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use mail_parser::{Address, HeaderValue, MessageParser, MimeHeaders, PartType};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};

use frankweiler_etl::blob_cas::{blake3_hex, BlobReader, BlobView, InMemoryBlobReader};

use crate::extract::db::{
    AttachmentRow, EmailJoins, EmailRow, LoadedAttachment, LoadedEmail, LoadedRaw,
};

/// Default account id when the caller doesn't supply one. Derived
/// from the mbox file's stem so a checked-in fixture is portable.
fn default_account_id(input_path: &Path) -> String {
    input_path
        .file_stem()
        .and_then(|s| s.to_str())
        .map(slugify)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "mbox".to_string())
}

fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut prev_dash = false;
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash && !out.is_empty() {
            out.push('_');
            prev_dash = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}

/// Walk `input_path` and parse every mbox found into one [`LoadedRaw`].
///
/// * If `input_path` is a file, treat it as one mbox.
/// * If it's a directory, recursively walk for `*.mbox` files and
///   concatenate them into one logical account.
/// * If it doesn't exist, return an empty [`LoadedRaw`] (matches the
///   contacts provider's translate-only "no fixture staged yet" shape).
///
/// `account_override`, when `Some`, replaces the file-stem default
/// for `account_id`. Callers that want a stable identity across
/// repathing wire this through from config.
pub fn parse(input_path: &Path, account_override: Option<&str>) -> Result<LoadedRaw> {
    let mut mboxes: Vec<PathBuf> = Vec::new();
    if input_path.is_file() {
        mboxes.push(input_path.to_path_buf());
    } else if input_path.is_dir() {
        collect_mbox_files(input_path, &mut mboxes)?;
    } else {
        return Ok(LoadedRaw::default());
    }
    mboxes.sort();

    let account_id = match account_override {
        Some(s) => s.to_string(),
        None => default_account_id(input_path),
    };

    let mut acc = Accumulator::new(account_id.clone());
    for path in &mboxes {
        let bytes = fs::read(path).with_context(|| format!("read mbox {}", path.display()))?;
        for raw in split_mbox(&bytes) {
            if let Err(e) = acc.ingest_message(&raw) {
                tracing::warn!(
                    event = "jmap_mbox_message_parse_failed",
                    path = %path.display(),
                    error = %e,
                );
            }
        }
    }

    Ok(acc.into_loaded(&account_id))
}

fn collect_mbox_files(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let entries = fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))?;
    for entry in entries {
        let entry = entry.with_context(|| format!("entry in {}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            collect_mbox_files(&path, out)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("mbox") {
            out.push(path);
        }
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────
// mbox framing
// ─────────────────────────────────────────────────────────────────────

/// Split an mbox blob into per-message raw byte slices.
///
/// RFC 4155 framing: messages are separated by lines starting with
/// `From ` (the literal five-byte token at the start of a line). We
/// split on those boundaries and discard the `From ` envelope line
/// itself before yielding the message bytes — mail-parser wants the
/// RFC 5322 message, not the mbox envelope.
///
/// Also unescapes mbox quoting: lines that begin with `>From ` (any
/// number of leading `>`s) get one `>` stripped, undoing the writer-
/// side escaping that prevents false message boundaries inside
/// bodies. RFC 4155 §3 specifies `mboxrd` style; Google Takeout uses
/// it.
pub fn split_mbox(body: &[u8]) -> Vec<Vec<u8>> {
    let mut out: Vec<Vec<u8>> = Vec::new();
    let mut current: Option<Vec<u8>> = None;
    let mut in_envelope = false;

    for line in iter_lines(body) {
        if is_from_line(line) {
            if let Some(buf) = current.take() {
                out.push(buf);
            }
            current = Some(Vec::with_capacity(4096));
            in_envelope = true;
            continue;
        }
        let Some(buf) = current.as_mut() else {
            continue;
        };
        if in_envelope {
            // The envelope line is the `From ...` line itself; once
            // we see a non-`From ` line we're into the headers.
            in_envelope = false;
        }
        // Strip one leading `>` from `>From ` (and `>>From `, etc).
        // Plain `>` lines (markdown quoting in the body) are
        // untouched — only `>+From ` is the escaped form.
        let unescaped = unescape_from_line(line);
        buf.extend_from_slice(&unescaped);
        buf.push(b'\n');
    }
    if let Some(buf) = current.take() {
        out.push(buf);
    }
    out
}

fn iter_lines(body: &[u8]) -> impl Iterator<Item = &[u8]> {
    body.split(|b| *b == b'\n').map(|line| {
        // Strip trailing `\r` for CRLF-normalized scans.
        if line.last() == Some(&b'\r') {
            &line[..line.len() - 1]
        } else {
            line
        }
    })
}

fn is_from_line(line: &[u8]) -> bool {
    line.len() >= 5 && &line[..5] == b"From "
}

fn unescape_from_line(line: &[u8]) -> Vec<u8> {
    // Count leading `>` then check if what follows is `From `.
    let n = line.iter().take_while(|b| **b == b'>').count();
    if n >= 1 && line.len() >= n + 5 && &line[n..n + 5] == b"From " {
        let mut v = Vec::with_capacity(line.len() - 1);
        v.extend_from_slice(&line[1..]);
        v
    } else {
        line.to_vec()
    }
}

// ─────────────────────────────────────────────────────────────────────
// Per-message ingestion
// ─────────────────────────────────────────────────────────────────────

struct Accumulator {
    account_id: String,
    /// label name → synthetic mailbox JSON. Built lazily as labels
    /// are encountered. The label name is preserved verbatim for the
    /// UI; the JMAP `role` is filled in for well-known labels.
    mailboxes: BTreeMap<String, MailboxEntry>,
    /// thread id → ordered list of (received_at_iso, email_id) for
    /// building the synthetic Thread.emailIds list in receivedAt
    /// order.
    threads: BTreeMap<String, Vec<(String, String)>>,
    emails: Vec<LoadedEmail>,
    joins_mailboxes: HashMap<String, Vec<String>>,
    joins_keywords: HashMap<String, Vec<String>>,
    joins_attachments: HashMap<String, Vec<LoadedAttachment>>,
    blobs: HashMap<String, BlobView>,
    /// Dedupe set so we never push the same email_id twice when an
    /// mbox accidentally contains duplicates (Takeout sometimes does
    /// for messages in multiple labels' mboxes — though the standard
    /// Takeout export is a single `All mail …` mbox where each
    /// message appears exactly once).
    seen_email_ids: BTreeSet<String>,
}

struct MailboxEntry {
    id: String,
    name: String,
    role: Option<&'static str>,
}

impl Accumulator {
    fn new(account_id: String) -> Self {
        Self {
            account_id,
            mailboxes: BTreeMap::new(),
            threads: BTreeMap::new(),
            emails: Vec::new(),
            joins_mailboxes: HashMap::new(),
            joins_keywords: HashMap::new(),
            joins_attachments: HashMap::new(),
            blobs: HashMap::new(),
            seen_email_ids: BTreeSet::new(),
        }
    }

    fn ingest_message(&mut self, raw: &[u8]) -> Result<()> {
        let parser = MessageParser::default();
        let msg = parser
            .parse(raw)
            .ok_or_else(|| anyhow::anyhow!("mail-parser returned None"))?;

        let eml_blob_id = sha256_hex(raw);
        let email_id = match msg.message_id() {
            Some(mid) => strip_angle(mid).to_string(),
            None => eml_blob_id.clone(),
        };
        if !self.seen_email_ids.insert(email_id.clone()) {
            return Ok(());
        }

        let thread_id = msg
            .header("X-GM-THRID")
            .and_then(header_text)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| email_id.clone());

        // Labels → mailbox ids + JMAP keyword set.
        let label_header = msg
            .header("X-Gmail-Labels")
            .and_then(header_text)
            .unwrap_or_default();
        let labels = split_gmail_labels(&label_header);
        let (mailbox_ids, keywords) = self.resolve_labels(&labels);

        // Body parts → JMAP-shape textBody / htmlBody + bodyValues.
        let (body_values, text_body, html_body) = build_body_parts(&msg);

        // Attachments → blob rows + AttachmentRow entries.
        let mut attachment_rows: Vec<AttachmentRow> = Vec::new();
        let mut loaded_attachments: Vec<LoadedAttachment> = Vec::new();
        let mut attachments_json: Vec<Value> = Vec::new();
        for (part_id, part) in iter_attachments(&msg) {
            let bytes = part.contents().to_vec();
            let blob_id = sha256_hex(&bytes);
            let name = part.attachment_name().map(str::to_string);
            let content_type = part.content_type().map(|ct| match ct.subtype() {
                Some(sub) => format!("{}/{}", ct.ctype(), sub),
                None => ct.ctype().to_string(),
            });
            let size = bytes.len() as i64;
            let disposition = part.content_disposition().map(|cd| cd.ctype().to_string());
            let cid = part.content_id().map(str::to_string);

            self.blobs.entry(blob_id.clone()).or_insert(BlobView {
                ref_id: blob_id.clone(),
                owning_id: email_id.clone(),
                slot: part_id.clone(),
                blake3: blake3_hex(&bytes),
                content_type: content_type.clone(),
                upstream_name: name.clone(),
                source_url: None,
                bytes,
            });

            attachments_json.push(json!({
                "partId": part_id,
                "blobId": blob_id,
                "name": name,
                "type": content_type,
                "size": size,
                "disposition": disposition,
                "cid": cid,
            }));
            attachment_rows.push(AttachmentRow {
                part_id: part_id.clone(),
                blob_id: blob_id.clone(),
                name: name.clone(),
                content_type: content_type.clone(),
                size: Some(size),
                disposition: disposition.clone(),
                cid: cid.clone(),
            });
            loaded_attachments.push(LoadedAttachment {
                part_id,
                blob_id,
                name,
                content_type,
                size: Some(size),
                disposition,
                cid,
            });
        }
        let has_attachment = !attachment_rows.is_empty();

        // .eml source blob.
        self.blobs.entry(eml_blob_id.clone()).or_insert(BlobView {
            ref_id: eml_blob_id.clone(),
            owning_id: email_id.clone(),
            slot: "source".to_string(),
            blake3: blake3_hex(raw),
            content_type: Some("message/rfc822".to_string()),
            upstream_name: None,
            source_url: None,
            bytes: raw.to_vec(),
        });

        // Date → ISO 8601.
        let received_at = msg
            .date()
            .map(|d| d.to_rfc3339())
            .or_else(|| header_text(msg.header("Date")?));
        let sent_at = received_at.clone();

        let subject = msg.subject().map(str::to_string);
        let from_json_val = addresses_to_jmap(msg.from());

        // Build the full payload JSON the render path consumes.
        let mailbox_ids_obj: Map<String, Value> = mailbox_ids
            .iter()
            .map(|id| (id.clone(), Value::Bool(true)))
            .collect();
        let keywords_obj: Map<String, Value> = keywords
            .iter()
            .map(|k| (k.clone(), Value::Bool(true)))
            .collect();
        let mut payload = json!({
            "id": email_id,
            "blobId": eml_blob_id,
            "threadId": thread_id,
            "mailboxIds": Value::Object(mailbox_ids_obj),
            "keywords": Value::Object(keywords_obj),
            "from": from_json_val,
            "to": addresses_to_jmap(msg.to()),
            "cc": addresses_to_jmap(msg.cc()),
            "bcc": addresses_to_jmap(msg.bcc()),
            "replyTo": addresses_to_jmap(msg.reply_to()),
            "subject": subject,
            "receivedAt": received_at,
            "sentAt": sent_at,
            "size": raw.len() as i64,
            "messageId": msg.message_id().map(|m| vec![strip_angle(m).to_string()]),
            "inReplyTo": header_msgid_list(msg.in_reply_to()),
            "references": header_msgid_list(msg.references()),
            "hasAttachment": has_attachment,
            "attachments": attachments_json,
            "preview": derive_preview(&body_values, &text_body, &html_body),
            "bodyValues": body_values,
            "textBody": text_body,
            "htmlBody": html_body,
        });
        // None values come through as null; trim them so the payload
        // looks like a real Email/get response (which just omits
        // absent fields).
        prune_nulls(&mut payload);

        let from_serialized = payload
            .get("from")
            .map(|v| serde_json::to_string(v).unwrap_or_default());

        let loaded = LoadedEmail {
            id: email_id.clone(),
            account_id: self.account_id.clone(),
            thread_id: thread_id.clone(),
            blob_id: eml_blob_id.clone(),
            message_id: msg.message_id().map(|m| strip_angle(m).to_string()),
            received_at: received_at.clone(),
            sent_at,
            size: Some(raw.len() as i64),
            subject,
            has_attachment,
            payload,
        };
        let _ = from_serialized; // not stored on LoadedEmail; render uses payload directly.

        self.threads
            .entry(thread_id.clone())
            .or_default()
            .push((received_at.unwrap_or_default(), email_id.clone()));

        self.joins_mailboxes.insert(email_id.clone(), mailbox_ids);
        self.joins_keywords.insert(email_id.clone(), keywords);
        if !loaded_attachments.is_empty() {
            self.joins_attachments
                .insert(email_id.clone(), loaded_attachments);
        }
        self.emails.push(loaded);
        Ok(())
    }

    /// Walk Gmail label strings, building/looking-up mailbox rows and
    /// computing the JMAP keyword set. Returns
    /// `(mailbox_ids, keywords)`.
    fn resolve_labels(&mut self, labels: &[String]) -> (Vec<String>, Vec<String>) {
        let mut mailbox_ids: Vec<String> = Vec::new();
        let mut keywords: BTreeSet<String> = BTreeSet::new();
        let mut is_unread = false;
        for label in labels {
            let trimmed = label.trim();
            if trimmed.is_empty() {
                continue;
            }
            match map_label(trimmed) {
                LabelMap::Mailbox { role } => {
                    let id = self.ensure_mailbox(trimmed, role);
                    if !mailbox_ids.contains(&id) {
                        mailbox_ids.push(id);
                    }
                }
                LabelMap::Keyword(kw) => {
                    keywords.insert(kw.to_string());
                }
                LabelMap::Unread => {
                    is_unread = true;
                }
                LabelMap::Drop => {}
            }
        }
        if !is_unread {
            keywords.insert("$seen".to_string());
        }
        (mailbox_ids, keywords.into_iter().collect())
    }

    fn ensure_mailbox(&mut self, name: &str, role: Option<&'static str>) -> String {
        if let Some(entry) = self.mailboxes.get(name) {
            return entry.id.clone();
        }
        let id = mailbox_id(&self.account_id, name);
        self.mailboxes.insert(
            name.to_string(),
            MailboxEntry {
                id: id.clone(),
                name: name.to_string(),
                role,
            },
        );
        id
    }

    fn into_loaded(mut self, account_id: &str) -> LoadedRaw {
        // Stable email order: (thread_id, received_at, id) matches
        // RawDb::load_emails so the renderer is identical to the
        // server-backed path.
        self.emails.sort_by(|a, b| {
            a.thread_id
                .cmp(&b.thread_id)
                .then_with(|| {
                    a.received_at
                        .as_deref()
                        .unwrap_or("")
                        .cmp(b.received_at.as_deref().unwrap_or(""))
                })
                .then_with(|| a.id.cmp(&b.id))
        });

        let accounts = vec![json!({
            "id": account_id,
            "name": account_id,
            "isPersonal": true,
        })];

        let mailboxes: Vec<Value> = self
            .mailboxes
            .values()
            .map(|m| {
                let mut obj = json!({
                    "id": m.id,
                    "name": m.name,
                });
                if let Some(role) = m.role {
                    obj["role"] = Value::String(role.to_string());
                }
                obj
            })
            .collect();

        let threads: Vec<Value> = self
            .threads
            .iter()
            .map(|(tid, members)| {
                let mut ordered = members.clone();
                ordered.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
                let ids: Vec<String> = ordered.into_iter().map(|(_, id)| id).collect();
                json!({
                    "id": tid,
                    "emailIds": ids,
                })
            })
            .collect();

        let joins = EmailJoins {
            mailboxes: self.joins_mailboxes,
            keywords: self.joins_keywords,
            attachments: self.joins_attachments,
        };

        let mut reader = InMemoryBlobReader::new();
        for (_id, view) in self.blobs {
            reader.insert(view);
        }
        let blobs: Arc<dyn BlobReader> = Arc::new(reader);

        LoadedRaw {
            accounts,
            mailboxes,
            threads,
            emails: self.emails,
            joins,
            blobs,
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Label mapping
// ─────────────────────────────────────────────────────────────────────

enum LabelMap {
    /// Promote to a mailbox row (optionally with a JMAP `role`).
    Mailbox { role: Option<&'static str> },
    /// Add a JMAP keyword (e.g. `$flagged`); no mailbox row.
    Keyword(&'static str),
    /// Special-case: clears the implicit `$seen` keyword.
    Unread,
    /// Discard — the label is a Gmail-internal state (e.g.
    /// `Archived` = "not in Inbox") with no JMAP analog.
    Drop,
}

fn map_label(label: &str) -> LabelMap {
    // Case-insensitive match on well-known labels; otherwise the
    // label becomes a plain mailbox with no role.
    let lower = label.to_ascii_lowercase();
    match lower.as_str() {
        "inbox" => LabelMap::Mailbox {
            role: Some("inbox"),
        },
        "sent" => LabelMap::Mailbox { role: Some("sent") },
        "drafts" | "draft" => LabelMap::Mailbox {
            role: Some("drafts"),
        },
        "trash" => LabelMap::Mailbox {
            role: Some("trash"),
        },
        "spam" | "junk" => LabelMap::Mailbox { role: Some("junk") },
        "all mail" => LabelMap::Mailbox {
            role: Some("archive"),
        },
        "starred" => LabelMap::Keyword("$flagged"),
        "important" => LabelMap::Keyword("$important"),
        "opened" | "read" => LabelMap::Keyword("$seen"),
        "unread" => LabelMap::Unread,
        "archived" => LabelMap::Drop,
        _ => LabelMap::Mailbox { role: None },
    }
}

/// Split an `X-Gmail-Labels` header value. Labels are
/// comma-separated; commas inside a label are backslash-escaped
/// (`\,`). Backslashes themselves are doubled (`\\`).
pub fn split_gmail_labels(value: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur = String::new();
    let mut chars = value.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\\' {
            if let Some(&next) = chars.peek() {
                cur.push(next);
                chars.next();
            }
            continue;
        }
        if c == ',' {
            out.push(cur.trim().to_string());
            cur.clear();
        } else {
            cur.push(c);
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out.retain(|s| !s.is_empty());
    out
}

fn mailbox_id(account_id: &str, label: &str) -> String {
    let mut h = Sha256::new();
    h.update(b"mbox:");
    h.update(account_id.as_bytes());
    h.update(b":");
    h.update(label.as_bytes());
    let digest = h.finalize();
    format!("mbox-{}", short_hex(&digest, 12))
}

// ─────────────────────────────────────────────────────────────────────
// mail-parser helpers
// ─────────────────────────────────────────────────────────────────────

/// Strip surrounding `<` / `>` from a header value like
/// `<abc@example.com>`. Leaves bare values untouched.
fn strip_angle(s: &str) -> &str {
    let t = s.trim();
    let t = t.strip_prefix('<').unwrap_or(t);
    t.strip_suffix('>').unwrap_or(t)
}

/// Best-effort text extraction from a generic `HeaderValue` —
/// strings, addresses, message-id lists all flatten to one string
/// for header-stash purposes.
fn header_text(hv: &HeaderValue) -> Option<String> {
    match hv {
        HeaderValue::Text(s) => Some(s.to_string()),
        HeaderValue::TextList(list) => Some(list.join(", ")),
        _ => None,
    }
}

/// JMAP `inReplyTo` / `references` are arrays of message-ids.
fn header_msgid_list(hv: &HeaderValue) -> Option<Vec<String>> {
    match hv {
        HeaderValue::Text(s) => Some(vec![strip_angle(s).to_string()]),
        HeaderValue::TextList(list) => Some(
            list.iter()
                .map(|s| strip_angle(s).to_string())
                .filter(|s| !s.is_empty())
                .collect(),
        ),
        _ => None,
    }
}

fn addresses_to_jmap(addr: Option<&Address>) -> Option<Vec<Value>> {
    let addr = addr?;
    let mut out: Vec<Value> = Vec::new();
    for a in addr.iter() {
        let email = a.address().unwrap_or_default().to_string();
        let name = a.name().map(str::to_string);
        if email.is_empty() && name.is_none() {
            continue;
        }
        let mut obj = serde_json::Map::new();
        if let Some(n) = name {
            obj.insert("name".into(), Value::String(n));
        }
        obj.insert("email".into(), Value::String(email));
        out.push(Value::Object(obj));
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

/// Walk every MIME part the message-parser surfaces as an attachment
/// or inline non-body part, yielding `(dotted_part_id, &MessagePart)`.
/// The dotted id is a stable identifier built from the parser's part
/// index — same input bytes always yield the same id.
fn iter_attachments<'a>(
    msg: &'a mail_parser::Message<'a>,
) -> impl Iterator<Item = (String, &'a mail_parser::MessagePart<'a>)> + 'a {
    // mail-parser numbers parts by their index in `msg.parts`; the
    // root is index 0. We expose ids as 1-based dotted paths to
    // match JMAP's `partId` convention (where index 0 is reserved
    // for the root message itself).
    let body_idx: std::collections::HashSet<usize> = msg
        .text_body
        .iter()
        .copied()
        .chain(msg.html_body.iter().copied())
        .collect();
    msg.attachments
        .iter()
        .copied()
        .chain(
            // Catch inline parts (images referenced by cid:) that
            // mail-parser may classify outside .attachments. We
            // dedupe via the set in the closure below.
            msg.html_body.iter().copied(),
        )
        // Remove duplicates while preserving the first occurrence's
        // ordering — and drop body parts so we don't surface
        // text/html as "attachments".
        .scan(std::collections::HashSet::new(), move |seen, idx| {
            if !seen.insert(idx) {
                return Some(None);
            }
            if body_idx.contains(&idx) {
                // It's a body part — only surface if it's an inline
                // image / has a cid. text/plain and text/html bodies
                // don't get attachment rows.
                let part = msg.part(idx)?;
                if part.content_id().is_some() {
                    return Some(Some((idx, part)));
                }
                return Some(None);
            }
            let part = msg.part(idx)?;
            Some(Some((idx, part)))
        })
        .flatten()
        .map(|(idx, part)| (format!("{}", idx + 1), part))
}

/// Build JMAP `bodyValues` + `textBody` + `htmlBody`. `bodyValues` is
/// keyed by partId (the same string we surface to
/// [`crate::translate::render`]). Text parts get UTF-8 decoded
/// contents inline; html parts get HTML inline.
fn build_body_parts(msg: &mail_parser::Message<'_>) -> (Value, Vec<Value>, Vec<Value>) {
    let mut body_values = Map::new();
    let mut text_body: Vec<Value> = Vec::new();
    let mut html_body: Vec<Value> = Vec::new();

    for &idx in &msg.text_body {
        let Some(part) = msg.part(idx) else { continue };
        let part_id = format!("{}", idx + 1);
        let value = part_text(part);
        body_values.insert(
            part_id.clone(),
            json!({
                "value": value,
                "isEncodingProblem": false,
                "isTruncated": false,
            }),
        );
        text_body.push(json!({
            "partId": part_id,
            "type": "text/plain",
        }));
    }
    for &idx in &msg.html_body {
        let Some(part) = msg.part(idx) else { continue };
        let part_id = format!("{}", idx + 1);
        body_values.entry(part_id.clone()).or_insert_with(|| {
            json!({
                "value": part_text(part),
                "isEncodingProblem": false,
                "isTruncated": false,
            })
        });
        html_body.push(json!({
            "partId": part_id,
            "type": "text/html",
        }));
    }

    (Value::Object(body_values), text_body, html_body)
}

fn part_text(part: &mail_parser::MessagePart<'_>) -> String {
    match &part.body {
        PartType::Text(s) | PartType::Html(s) => s.to_string(),
        PartType::Binary(b) | PartType::InlineBinary(b) => String::from_utf8_lossy(b).into_owned(),
        _ => String::new(),
    }
}

/// Derive a JMAP-style `preview` snippet (first ~200 chars of plain
/// text, falling back to stripped HTML). Render uses this only when
/// neither body part renders to anything.
fn derive_preview(body_values: &Value, text_body: &[Value], html_body: &[Value]) -> String {
    let pick = |arr: &[Value]| -> Option<String> {
        let first = arr.first()?.get("partId")?.as_str()?;
        let bv = body_values.get(first)?.get("value")?.as_str()?;
        Some(bv.to_string())
    };
    let raw = pick(text_body)
        .or_else(|| pick(html_body))
        .unwrap_or_default();
    let collapsed: String = raw
        .chars()
        .map(|c| if c.is_whitespace() { ' ' } else { c })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    collapsed.chars().take(200).collect()
}

/// Walk the JSON tree pruning `null` leaves so the synthetic payload
/// shape resembles a real Email/get response (which omits absent
/// fields rather than returning explicit nulls).
fn prune_nulls(v: &mut Value) {
    match v {
        Value::Object(map) => {
            let to_remove: Vec<String> = map
                .iter()
                .filter(|(_, v)| v.is_null())
                .map(|(k, _)| k.clone())
                .collect();
            for k in to_remove {
                map.remove(&k);
            }
            for v in map.values_mut() {
                prune_nulls(v);
            }
        }
        Value::Array(arr) => {
            for v in arr {
                prune_nulls(v);
            }
        }
        _ => {}
    }
}

// ─────────────────────────────────────────────────────────────────────
// hash helpers
// ─────────────────────────────────────────────────────────────────────

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let d = h.finalize();
    let mut out = String::with_capacity(d.len() * 2);
    for b in d.iter() {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

fn short_hex(bytes: &[u8], n: usize) -> String {
    let mut out = String::with_capacity(n * 2);
    for b in bytes.iter().take(n) {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

// Silence unused-import warnings for things we re-export below for
// downstream consumers but don't reference in this file.
#[allow(dead_code)]
fn _force_use_unused(_e: EmailRow) {}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const TWO_MSG_MBOX: &str = concat!(
        "From 1111@xxx Wed Jun 03 22:30:48 +0000 2026\n",
        "X-GM-THRID: 1111\n",
        "X-Gmail-Labels: Inbox,Starred,Unread\n",
        "Message-Id: <msg-one@enterprise.starfleet>\n",
        "From: Jean-Luc Picard <picard@enterprise.starfleet>\n",
        "To: William Riker <riker@enterprise.starfleet>\n",
        "Subject: Make it so\n",
        "Date: Wed, 3 Jun 2026 22:30:47 +0000\n",
        "Content-Type: text/plain; charset=utf-8\n",
        "\n",
        "Number One, set a course for Risa.\n",
        "\n",
        "From 2222@xxx Wed Jun 03 23:00:00 +0000 2026\n",
        "X-GM-THRID: 1111\n",
        "X-Gmail-Labels: Inbox,Sent\n",
        "Message-Id: <msg-two@enterprise.starfleet>\n",
        "In-Reply-To: <msg-one@enterprise.starfleet>\n",
        "From: William Riker <riker@enterprise.starfleet>\n",
        "To: Jean-Luc Picard <picard@enterprise.starfleet>\n",
        "Subject: Re: Make it so\n",
        "Date: Wed, 3 Jun 2026 23:00:00 +0000\n",
        "Content-Type: text/plain; charset=utf-8\n",
        "\n",
        "Aye, sir. Course laid in.\n",
    );

    fn parse_str(body: &str) -> LoadedRaw {
        let tmp = tempfile::NamedTempFile::new().unwrap();
        std::fs::write(tmp.path(), body).unwrap();
        // Rename so the file stem is predictable.
        let stable = tmp.path().with_file_name("trek.mbox");
        std::fs::rename(tmp.path(), &stable).unwrap();
        parse(&stable, None).unwrap()
    }

    #[test]
    fn split_mbox_finds_two_messages() {
        let parts = split_mbox(TWO_MSG_MBOX.as_bytes());
        assert_eq!(parts.len(), 2);
        assert!(parts[0].starts_with(b"X-GM-THRID:"));
        assert!(parts[1].starts_with(b"X-GM-THRID:"));
    }

    #[test]
    fn unescape_strips_one_gt_from_quoted_from_lines() {
        let body = b"From 1@x Wed Jun 03 22:30:48 +0000 2026\nSubject: t\n\n>From the desk of...\n>>From: not-a-header\nbody\n";
        let parts = split_mbox(body);
        assert_eq!(parts.len(), 1);
        let s = std::str::from_utf8(&parts[0]).unwrap();
        assert!(s.contains("From the desk"));
        assert!(s.contains(">From: not-a-header"));
    }

    #[test]
    fn stable_email_id_from_message_id() {
        let raw1 = parse_str(TWO_MSG_MBOX);
        let raw2 = parse_str(TWO_MSG_MBOX);
        assert_eq!(raw1.emails.len(), 2);
        assert_eq!(raw2.emails.len(), 2);
        for (a, b) in raw1.emails.iter().zip(raw2.emails.iter()) {
            assert_eq!(a.id, b.id);
            assert_eq!(a.thread_id, b.thread_id);
            assert_eq!(a.blob_id, b.blob_id);
        }
        // Picard's message id strips the angle brackets.
        let picard = raw1
            .emails
            .iter()
            .find(|e| e.subject.as_deref() == Some("Make it so"))
            .unwrap();
        assert_eq!(picard.id, "msg-one@enterprise.starfleet");
    }

    #[test]
    fn xgmthrid_groups_emails_into_one_thread() {
        let raw = parse_str(TWO_MSG_MBOX);
        let tids: BTreeSet<_> = raw.emails.iter().map(|e| e.thread_id.clone()).collect();
        assert_eq!(tids.len(), 1);
        assert_eq!(raw.threads.len(), 1);
        let thread = &raw.threads[0];
        assert_eq!(thread["id"], "1111");
        let ids = thread["emailIds"].as_array().unwrap();
        // receivedAt-ordered: Picard first, Riker second.
        assert_eq!(ids[0], "msg-one@enterprise.starfleet");
        assert_eq!(ids[1], "msg-two@enterprise.starfleet");
    }

    #[test]
    fn labels_promote_to_mailboxes_with_jmap_roles() {
        let raw = parse_str(TWO_MSG_MBOX);
        let by_name: HashMap<String, &Value> = raw
            .mailboxes
            .iter()
            .map(|m| (m["name"].as_str().unwrap().to_string(), m))
            .collect();
        assert_eq!(by_name["Inbox"]["role"], "inbox");
        assert_eq!(by_name["Sent"]["role"], "sent");
        // Starred is a keyword, not a mailbox.
        assert!(!by_name.contains_key("Starred"));
    }

    #[test]
    fn unread_label_suppresses_seen_keyword() {
        let raw = parse_str(TWO_MSG_MBOX);
        let picard = raw
            .emails
            .iter()
            .find(|e| e.subject.as_deref() == Some("Make it so"))
            .unwrap();
        let kws = &raw.joins.keywords[&picard.id];
        // Picard's message had `Unread` → no $seen, but has $flagged.
        assert!(!kws.iter().any(|k| k == "$seen"));
        assert!(kws.iter().any(|k| k == "$flagged"));

        let riker = raw
            .emails
            .iter()
            .find(|e| e.subject.as_deref() == Some("Re: Make it so"))
            .unwrap();
        let kws = &raw.joins.keywords[&riker.id];
        // Riker's message had no `Unread` → $seen implied.
        assert!(kws.iter().any(|k| k == "$seen"));
    }

    #[test]
    fn split_gmail_labels_unescapes_commas() {
        let labels = split_gmail_labels(r"Inbox,Personal\, Custom,Starred");
        assert_eq!(labels, vec!["Inbox", "Personal, Custom", "Starred"]);
    }

    #[test]
    fn empty_path_returns_empty_loaded() {
        let raw = parse(Path::new("/does/not/exist"), None).unwrap();
        assert!(raw.emails.is_empty());
    }
}
