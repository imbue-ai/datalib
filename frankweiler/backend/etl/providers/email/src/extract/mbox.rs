//! mbox extractor. Walks a Google Takeout `.mbox` file (RFC 4155
//! mboxrd framing, with `X-GM-THRID` / `X-Gmail-Labels` Gmail
//! extensions) and lands every message into the shared email raw
//! store as if it had come off a JMAP server — typed envelope
//! columns + join rows + the RFC 5322 `.eml` bytes in the blob CAS.
//! No body parsing, no html2md, no JMAP-shape payload synthesis;
//! translate handles all of that downstream off the `.eml` blob.
//!
//! ## Stable identifiers
//!
//! Re-ingesting the same mbox produces byte-identical rows. All ids
//! derive from the message contents or its mbox-level location:
//!
//!   * `account_id` — file stem of the mbox (e.g.
//!     `all_mail_including_spam_and_trash`), or the caller-supplied
//!     override.
//!   * `email_id` (= `emails.id`) — the `Message-Id` header verbatim
//!     (angle brackets stripped), falling back to
//!     `sha256(raw_eml_bytes)` hex when the header is missing.
//!   * `thread_id` — `X-GM-THRID` verbatim. Falls back to the email's
//!     own id (a single-message thread) when absent.
//!   * `mailbox_id` — short hex `sha256("mbox:" + account + ":" +
//!     label_name)`.
//!   * `email.blob_id` — `sha256(raw_eml_bytes)` hex; same value the
//!     blob CAS uses as its ref_id.
//!   * `attachment.part_id` — the dotted MIME part path
//!     (`"2"`, `"2.1"`, …); deterministic from the message tree.
//!   * `attachment.blob_id` — `sha256(bytes)` hex.
//!
//! ## Gmail label → JMAP `role` / keyword mapping
//!
//! Google Takeout writes a comma-separated `X-Gmail-Labels` header
//! per message. We line them up with JMAP's standard mailbox roles
//! where possible:
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
//! | (any other user label)       | role=`null`, name kept      |

use std::collections::{BTreeMap, BTreeSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use frankweiler_etl::blob_cas::blake3_hex;
use frankweiler_etl::control::ExtractControl;
use frankweiler_etl::progress::Progress;
use mail_parser::{Address, HeaderValue, MessageParser, MimeHeaders, PartType};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};
use sqlx::{Sqlite, Transaction};
use tracing::warn;

use super::db::{db_path_for, AttachmentRow, EmailRow, RawDb};
use super::schema_raw::{BLOB_KIND_ATTACHMENT, BLOB_KIND_EML};

/// Maximum emails accumulated in memory before we flush a bulk batch
/// to disk. Keeps peak RSS bounded while still amortizing doltlite's
/// per-transaction manifest-mutation cost across many rows.
///
/// Each entity-pool flush is one `BEGIN ... COMMIT` containing chunked
/// multi-row `INSERT`s for `emails` + each join table + `blob_refs` +
/// bookkeeping. The matching CAS-pool flush is one `BEGIN ... COMMIT`
/// containing chunked multi-row `INSERT`s for `cas_objects`. Two
/// transactions per batch instead of ~7 per email — at 17k emails
/// that's ~30 transactions instead of ~120k.
const FLUSH_BATCH: usize = 2000;

/// Rows per multi-row `INSERT` statement. Well under SQLite's default
/// 32k parameter ceiling for the widest row shape (`emails`, 11 cols
/// → 4400 binds per statement at this chunk size).
const SQL_CHUNK: usize = 400;

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Doltlite database path. Ignored when `db` is `Some`.
    pub db_path: PathBuf,
    /// Pre-opened raw DB (sync orchestrator populates this so the
    /// post-extract commit hits the same pool).
    pub db: Option<RawDb>,
    /// `.mbox` file (or directory containing `*.mbox` files).
    pub input_path: PathBuf,
    /// Overrides the file-stem default for `account_id`.
    pub account_id_override: Option<String>,
    /// Skip attachment bytes whose size exceeds this. The
    /// `email_attachments` row still lands (so we record what was
    /// referenced), but the bytes never enter the CAS — translate
    /// will render `_(blob not materialized)_` for them.
    pub blob_size_limit_bytes: Option<u64>,
    pub progress: Progress,
    pub control: ExtractControl,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            db_path: PathBuf::new(),
            db: None,
            input_path: PathBuf::new(),
            account_id_override: None,
            blob_size_limit_bytes: None,
            progress: Progress::noop(),
            control: ExtractControl::default(),
        }
    }
}

#[derive(Debug, Default, Serialize, Clone)]
pub struct FetchSummary {
    pub mailboxes_upserted: usize,
    pub threads_upserted: usize,
    pub emails_upserted: usize,
    pub blobs_stored: usize,
    pub blobs_skipped: usize,
    pub blobs_oversize: usize,
    pub parse_errors: usize,
}

/// Walk `opts.input_path` and land every message into the raw store
/// via in-memory accumulation + chunked multi-row `INSERT`s — see
/// [`FLUSH_BATCH`] for the per-batch-flush shape.
pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db = match opts.db.clone() {
        Some(db) => db,
        None => RawDb::open(&db_path_for(&opts.input_path)).await?,
    };
    if opts.control.reset_and_redownload {
        db.reset().await?;
    }

    let mbox_paths = collect_mbox_files(&opts.input_path)?;
    if mbox_paths.is_empty() {
        return Ok(FetchSummary::default());
    }
    let account_id = opts
        .account_id_override
        .clone()
        .unwrap_or_else(|| default_account_id(&opts.input_path));

    let known_blobs = db.loaded_blob_ids().await?;

    let mut accumulator = Accumulator::new(account_id.clone(), opts.blob_size_limit_bytes);
    let mut summary = FetchSummary::default();
    let mut batch = PendingBatch::default();

    for path in &mbox_paths {
        for raw in iter_mbox_messages(path)? {
            let raw = match raw {
                Ok(bytes) => bytes,
                Err(e) => {
                    warn!(event = "mbox_read_failed", path = %path.display(), error = %e);
                    summary.parse_errors += 1;
                    continue;
                }
            };
            match accumulator.ingest_message(&raw, &known_blobs, &mut batch, &mut summary) {
                Ok(true) => {
                    if batch.emails.len() >= FLUSH_BATCH {
                        flush_batch(&db, &mut batch, &opts.progress, &mut summary).await?;
                    }
                }
                Ok(false) => {} // duplicate; skipped
                Err(e) => {
                    warn!(event = "mbox_message_failed", error = %e);
                    summary.parse_errors += 1;
                }
            }
        }
    }
    flush_batch(&db, &mut batch, &opts.progress, &mut summary).await?;

    // Account + mailboxes + threads + matching bookkeeping all land in
    // one closing transaction. They're tiny compared to the email
    // tables (mailbox count = label count ~ tens; thread count up to
    // total emails / avg-thread-size).
    flush_account_and_lookups(&db, &account_id, &accumulator, &mut summary).await?;

    Ok(summary)
}

// ─────────────────────────────────────────────────────────────────────
// Streaming mbox iterator
// ─────────────────────────────────────────────────────────────────────

/// Iterate `path` yielding one RFC 5322 message at a time. The mbox
/// envelope `From ` line is stripped; `>From `-style escapes are
/// unquoted. Streams off disk via `BufReader` so peak RSS stays bounded
/// regardless of file size.
fn iter_mbox_messages(path: &Path) -> Result<impl Iterator<Item = Result<Vec<u8>>>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::with_capacity(1 << 16, file);
    let mut pending: Option<Vec<u8>> = None;
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut started = false;
    let it = std::iter::from_fn(move || loop {
        buf.clear();
        let n = match reader.read_until(b'\n', &mut buf) {
            Ok(0) => {
                // EOF; flush any pending message.
                return pending.take().map(Ok);
            }
            Ok(n) => n,
            Err(e) => return Some(Err(e.into())),
        };
        // Strip trailing newline (and CR if CRLF).
        let mut line: &[u8] = &buf[..n];
        if line.last() == Some(&b'\n') {
            line = &line[..line.len() - 1];
            if line.last() == Some(&b'\r') {
                line = &line[..line.len() - 1];
            }
        }
        if is_from_line(line) {
            let prev = pending.take();
            pending = Some(Vec::with_capacity(4096));
            started = true;
            if let Some(msg) = prev {
                return Some(Ok(msg));
            }
            continue;
        }
        if !started {
            // Tolerate leading junk before the first `From ` line.
            continue;
        }
        let target = pending.as_mut().expect("started => Some");
        let unescaped = unescape_from_line(line);
        target.extend_from_slice(&unescaped);
        target.push(b'\n');
    });
    Ok(it)
}

fn is_from_line(line: &[u8]) -> bool {
    line.len() >= 5 && &line[..5] == b"From "
}

/// Strip one leading `>` from `>From ` (and `>>From `, etc).
fn unescape_from_line(line: &[u8]) -> Vec<u8> {
    let n = line.iter().take_while(|b| **b == b'>').count();
    if n >= 1 && line.len() >= n + 5 && &line[n..n + 5] == b"From " {
        line[1..].to_vec()
    } else {
        line.to_vec()
    }
}

// ─────────────────────────────────────────────────────────────────────
// Per-message envelope extraction
// ─────────────────────────────────────────────────────────────────────

struct Accumulator {
    account_id: String,
    blob_size_limit_bytes: Option<u64>,
    mailboxes: BTreeMap<String, MailboxEntry>,
    threads: BTreeMap<String, Vec<ThreadMember>>,
    seen_email_ids: BTreeSet<String>,
}

struct MailboxEntry {
    id: String,
    role: Option<&'static str>,
}

#[derive(Clone)]
struct ThreadMember {
    id: String,
    received: String,
}

impl Accumulator {
    fn new(account_id: String, blob_size_limit_bytes: Option<u64>) -> Self {
        Self {
            account_id,
            blob_size_limit_bytes,
            mailboxes: BTreeMap::new(),
            threads: BTreeMap::new(),
            seen_email_ids: BTreeSet::new(),
        }
    }

    /// Parse one message's envelope + MIME structure, stash the row
    /// and any blob bytes into `pending`, and update `summary`'s
    /// counters. Returns `Ok(true)` when a new row was pushed,
    /// `Ok(false)` when the message was a duplicate of one we've
    /// already seen in this run.
    fn ingest_message(
        &mut self,
        raw: &[u8],
        known_blobs: &std::collections::HashSet<String>,
        pending: &mut PendingBatch,
        summary: &mut FetchSummary,
    ) -> Result<bool> {
        let msg = MessageParser::default()
            .parse(raw)
            .ok_or_else(|| anyhow!("mail-parser returned None"))?;

        // One hash per .eml: blake3 over the raw bytes is both the
        // CAS key and (for ref-id / fallback email-id purposes) the
        // content-addressed identifier. sha256 was a profile hotspot
        // on Apple Silicon (no ARMv8 hardware accel in the `sha2`
        // crate), and hashing every message twice was pure waste.
        let eml_blake3 = blake3_hex(raw);
        let eml_blob_id = eml_blake3.clone();
        let email_id = match msg.message_id() {
            Some(mid) => strip_angle(mid).to_string(),
            None => eml_blob_id.clone(),
        };
        if !self.seen_email_ids.insert(email_id.clone()) {
            return Ok(false);
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

        // Date / subject / from.
        let received_at = msg
            .date()
            .and_then(|d| frankweiler_time::parse_strict(&d.to_rfc3339()).ok())
            .map(|t| t.to_rfc3339())
            .or_else(|| header_text(msg.header("Date")?));
        let sent_at = received_at.clone();
        let subject = msg.subject().map(str::to_string);
        let from_json =
            addresses_to_jmap(msg.from()).map(|v| serde_json::to_string(&v).unwrap_or_default());

        // Walk the MIME tree once to enumerate attachments + queue
        // their bytes for the CAS bulk-insert. No body decode.
        let mut attachments: Vec<AttachmentRow> = Vec::new();
        for (part_id, part) in iter_attachments(&msg) {
            let bytes = part.contents();
            let blob_blake3 = blake3_hex(bytes);
            let blob_id = blob_blake3.clone();
            let name = part.attachment_name().map(str::to_string);
            let content_type = part.content_type().map(|ct| match ct.subtype() {
                Some(sub) => format!("{}/{}", ct.ctype(), sub),
                None => ct.ctype().to_string(),
            });
            let size = bytes.len() as i64;
            let disposition = part.content_disposition().map(|cd| cd.ctype().to_string());
            let cid = part.content_id().map(str::to_string);

            let oversize = self
                .blob_size_limit_bytes
                .is_some_and(|cap| bytes.len() as u64 > cap);
            if oversize {
                summary.blobs_oversize += 1;
            } else if known_blobs.contains(&blob_id) || pending.seen_blob_ids.contains(&blob_id) {
                summary.blobs_skipped += 1;
            } else {
                pending.seen_blob_ids.insert(blob_id.clone());
                pending.cas_objects.push(PendingCas {
                    blake3: blob_blake3,
                    ref_id: blob_id.clone(),
                    kind: BLOB_KIND_ATTACHMENT,
                    owning_id: email_id.clone(),
                    slot: part_id.clone(),
                    upstream_name: name.clone(),
                    content_type: content_type.clone(),
                    bytes: bytes.to_vec(),
                });
                summary.blobs_stored += 1;
            }

            attachments.push(AttachmentRow {
                part_id,
                blob_id,
                name,
                content_type,
                size: Some(size),
                disposition,
                cid,
            });
        }
        let has_attachment = !attachments.is_empty();

        // Queue the .eml itself.
        if known_blobs.contains(&eml_blob_id) || pending.seen_blob_ids.contains(&eml_blob_id) {
            summary.blobs_skipped += 1;
        } else {
            pending.seen_blob_ids.insert(eml_blob_id.clone());
            pending.cas_objects.push(PendingCas {
                blake3: eml_blake3,
                ref_id: eml_blob_id.clone(),
                kind: BLOB_KIND_EML,
                owning_id: email_id.clone(),
                slot: "source".to_string(),
                upstream_name: None,
                content_type: Some("message/rfc822".to_string()),
                bytes: raw.to_vec(),
            });
            summary.blobs_stored += 1;
        }

        self.threads
            .entry(thread_id.clone())
            .or_default()
            .push(ThreadMember {
                id: email_id.clone(),
                received: received_at.clone().unwrap_or_default(),
            });

        pending.emails.push(EmailRow {
            id: email_id,
            account_id: self.account_id.clone(),
            thread_id,
            blob_id: eml_blob_id,
            message_id: msg.message_id().map(|m| strip_angle(m).to_string()),
            received_at,
            sent_at,
            size: Some(raw.len() as i64),
            subject,
            from_json,
            has_attachment,
            mailbox_ids,
            keywords,
            attachments,
        });
        Ok(true)
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
                role,
            },
        );
        id
    }
}

// ─────────────────────────────────────────────────────────────────────
// Bulk-write flush path
// ─────────────────────────────────────────────────────────────────────

/// Everything the next flush will hand to doltlite. Accumulating in
/// memory and then flushing as one entity-pool transaction + one
/// CAS-pool transaction is dramatically cheaper than per-row writes:
/// doltlite charges a prolly-tree manifest mutation per `BEGIN ...
/// COMMIT`, so going from ~7 transactions per email to ~2 per
/// `FLUSH_BATCH` cuts orders of magnitude off ingest time.
#[derive(Default)]
struct PendingBatch {
    emails: Vec<EmailRow>,
    cas_objects: Vec<PendingCas>,
    /// In-run dedupe of blob ref ids. JMAP `Email.blobId` is server-
    /// opaque (different per email), but for mbox sources the ref_id
    /// is `sha256(bytes)` — identical bodies / attachments collapse
    /// to a single row, and this set keeps the `INSERT` list itself
    /// dedup-free so doltlite never sees a conflicting bind pair
    /// inside one multi-row statement.
    seen_blob_ids: std::collections::HashSet<String>,
}

struct PendingCas {
    blake3: String,
    ref_id: String,
    kind: &'static str,
    owning_id: String,
    slot: String,
    upstream_name: Option<String>,
    content_type: Option<String>,
    bytes: Vec<u8>,
}

impl PendingBatch {
    fn clear(&mut self) {
        self.emails.clear();
        self.cas_objects.clear();
        // `seen_blob_ids` deliberately persists across flushes: an
        // identical attachment landing in a later batch should still
        // dedupe against an earlier flush in the same run.
    }
}

/// Flush one accumulated `PendingBatch` to disk: chunked multi-row
/// `INSERT`s inside a single entity-pool transaction (emails + join
/// tables + blob_refs + bookkeeping) plus a single CAS-pool
/// transaction (cas_objects).
async fn flush_batch(
    db: &RawDb,
    batch: &mut PendingBatch,
    progress: &Progress,
    summary: &mut FetchSummary,
) -> Result<()> {
    if batch.emails.is_empty() && batch.cas_objects.is_empty() {
        return Ok(());
    }
    let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();

    let mut etx = db.pool().begin().await.context("begin entity tx")?;
    bulk_insert_emails(&mut etx, &batch.emails).await?;
    bulk_insert_emails_bookkeeping(&mut etx, &batch.emails, &now).await?;
    bulk_insert_email_mailboxes(&mut etx, &batch.emails).await?;
    bulk_insert_email_keywords(&mut etx, &batch.emails).await?;
    bulk_insert_email_attachments(&mut etx, &batch.emails).await?;
    bulk_insert_blob_refs(&mut etx, &batch.cas_objects).await?;
    bulk_insert_blob_refs_bookkeeping(&mut etx, &batch.cas_objects, &now).await?;
    etx.commit().await.context("commit entity tx")?;

    if !batch.cas_objects.is_empty() {
        let mut ctx = db.cas().pool().begin().await.context("begin cas tx")?;
        bulk_insert_cas_objects(&mut ctx, &batch.cas_objects, &now).await?;
        ctx.commit().await.context("commit cas tx")?;
    }

    summary.emails_upserted += batch.emails.len();
    progress.inc(batch.emails.len() as u64);
    batch.clear();
    Ok(())
}

/// Flush the account row, the per-label mailbox rows, and the per-
/// thread rows once the message walk is done. Three small tables;
/// one transaction.
async fn flush_account_and_lookups(
    db: &RawDb,
    account_id: &str,
    accumulator: &Accumulator,
    summary: &mut FetchSummary,
) -> Result<()> {
    let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    let mut tx = db.pool().begin().await.context("begin lookups tx")?;

    // Account.
    let account_payload = serde_json::to_string(&serde_json::json!({
        "id": account_id,
        "name": account_id,
        "isPersonal": true,
    }))
    .unwrap_or_default();
    sqlx::query(
        "INSERT INTO accounts (id, name, is_personal, is_read_only, payload)
         VALUES (?, ?, 1, NULL, jsonb(?))
         ON CONFLICT(id) DO UPDATE SET
            name = excluded.name,
            is_personal = excluded.is_personal,
            payload = excluded.payload",
    )
    .bind(account_id)
    .bind(account_id)
    .bind(&account_payload)
    .execute(&mut *tx)
    .await
    .context("insert account")?;
    bulk_insert_bookkeeping_for_ids(&mut tx, "accounts", std::iter::once(account_id), &now).await?;

    // Mailboxes.
    let mailbox_specs: Vec<(String, String, Option<&'static str>, String)> = accumulator
        .mailboxes
        .iter()
        .map(|(name, entry)| {
            let payload = match entry.role {
                Some(role) => serde_json::json!({
                    "id": entry.id,
                    "name": name,
                    "role": role,
                }),
                None => serde_json::json!({"id": entry.id, "name": name}),
            };
            (
                entry.id.clone(),
                name.clone(),
                entry.role,
                serde_json::to_string(&payload).unwrap_or_default(),
            )
        })
        .collect();
    bulk_insert_mailboxes(&mut tx, account_id, &mailbox_specs).await?;
    bulk_insert_bookkeeping_for_ids(
        &mut tx,
        "mailboxes",
        mailbox_specs.iter().map(|(id, _, _, _)| id.as_str()),
        &now,
    )
    .await?;
    summary.mailboxes_upserted = mailbox_specs.len();

    // Threads — emailIds ordered by (receivedAt, id) for byte-stable
    // payloads across re-ingests.
    let mut thread_specs: Vec<(String, i64, String)> =
        Vec::with_capacity(accumulator.threads.len());
    for (tid, members) in &accumulator.threads {
        let mut ordered = members.to_vec();
        ordered.sort_by(|a, b| a.received.cmp(&b.received).then_with(|| a.id.cmp(&b.id)));
        let ids: Vec<String> = ordered.into_iter().map(|m| m.id).collect();
        let count = ids.len() as i64;
        let payload = serde_json::to_string(&serde_json::json!({"id": tid, "emailIds": ids}))
            .unwrap_or_default();
        thread_specs.push((tid.clone(), count, payload));
    }
    bulk_insert_threads(&mut tx, account_id, &thread_specs).await?;
    bulk_insert_bookkeeping_for_ids(
        &mut tx,
        "threads",
        thread_specs.iter().map(|(id, _, _)| id.as_str()),
        &now,
    )
    .await?;
    summary.threads_upserted = thread_specs.len();

    tx.commit().await.context("commit lookups tx")?;
    Ok(())
}

async fn bulk_insert_emails(tx: &mut Transaction<'_, Sqlite>, rows: &[EmailRow]) -> Result<()> {
    let cols = 11;
    for chunk in rows.chunks(SQL_CHUNK) {
        let mut sql = String::from(
            "INSERT INTO emails
                (id, account_id, thread_id, blob_id, message_id, received_at, sent_at,
                 size, subject, from_json, has_attachment)
             VALUES ",
        );
        push_placeholders(&mut sql, chunk.len(), cols);
        sql.push_str(
            " ON CONFLICT(id) DO UPDATE SET
                account_id = excluded.account_id,
                thread_id = excluded.thread_id,
                blob_id = excluded.blob_id,
                message_id = COALESCE(excluded.message_id, emails.message_id),
                received_at = COALESCE(excluded.received_at, emails.received_at),
                sent_at = COALESCE(excluded.sent_at, emails.sent_at),
                size = COALESCE(excluded.size, emails.size),
                subject = COALESCE(excluded.subject, emails.subject),
                from_json = COALESCE(excluded.from_json, emails.from_json),
                has_attachment = COALESCE(excluded.has_attachment, emails.has_attachment)",
        );
        let mut q = sqlx::query(&sql);
        for row in chunk {
            q = q
                .bind(&row.id)
                .bind(&row.account_id)
                .bind(&row.thread_id)
                .bind(&row.blob_id)
                .bind(row.message_id.as_deref())
                .bind(row.received_at.as_deref())
                .bind(row.sent_at.as_deref())
                .bind(row.size)
                .bind(row.subject.as_deref())
                .bind(row.from_json.as_deref())
                .bind(row.has_attachment as i64);
        }
        q.execute(&mut **tx).await.context("bulk insert emails")?;
    }
    Ok(())
}

async fn bulk_insert_emails_bookkeeping(
    tx: &mut Transaction<'_, Sqlite>,
    rows: &[EmailRow],
    now: &str,
) -> Result<()> {
    bulk_insert_bookkeeping_for_ids(tx, "emails", rows.iter().map(|r| r.id.as_str()), now).await
}

async fn bulk_insert_email_mailboxes(
    tx: &mut Transaction<'_, Sqlite>,
    rows: &[EmailRow],
) -> Result<()> {
    // delete-then-insert: the source-of-truth set for this email
    // comes from this run, not whatever was on disk before.
    for chunk in rows.chunks(SQL_CHUNK) {
        let mut sql = String::from("DELETE FROM email_mailboxes WHERE email_id IN (");
        push_placeholder_list(&mut sql, chunk.len());
        sql.push(')');
        let mut q = sqlx::query(&sql);
        for r in chunk {
            q = q.bind(&r.id);
        }
        q.execute(&mut **tx)
            .await
            .context("bulk delete email_mailboxes")?;
    }
    let pairs: Vec<(&str, &str)> = rows
        .iter()
        .flat_map(|r| {
            r.mailbox_ids
                .iter()
                .map(move |m| (r.id.as_str(), m.as_str()))
        })
        .collect();
    for chunk in pairs.chunks(SQL_CHUNK) {
        let mut sql = String::from("INSERT INTO email_mailboxes (email_id, mailbox_id) VALUES ");
        push_placeholders(&mut sql, chunk.len(), 2);
        sql.push_str(" ON CONFLICT(email_id, mailbox_id) DO NOTHING");
        let mut q = sqlx::query(&sql);
        for (eid, mid) in chunk {
            q = q.bind(eid).bind(mid);
        }
        q.execute(&mut **tx)
            .await
            .context("bulk insert email_mailboxes")?;
    }
    Ok(())
}

async fn bulk_insert_email_keywords(
    tx: &mut Transaction<'_, Sqlite>,
    rows: &[EmailRow],
) -> Result<()> {
    for chunk in rows.chunks(SQL_CHUNK) {
        let mut sql = String::from("DELETE FROM email_keywords WHERE email_id IN (");
        push_placeholder_list(&mut sql, chunk.len());
        sql.push(')');
        let mut q = sqlx::query(&sql);
        for r in chunk {
            q = q.bind(&r.id);
        }
        q.execute(&mut **tx)
            .await
            .context("bulk delete email_keywords")?;
    }
    let pairs: Vec<(&str, &str)> = rows
        .iter()
        .flat_map(|r| r.keywords.iter().map(move |k| (r.id.as_str(), k.as_str())))
        .collect();
    for chunk in pairs.chunks(SQL_CHUNK) {
        let mut sql = String::from("INSERT INTO email_keywords (email_id, keyword) VALUES ");
        push_placeholders(&mut sql, chunk.len(), 2);
        sql.push_str(" ON CONFLICT(email_id, keyword) DO NOTHING");
        let mut q = sqlx::query(&sql);
        for (eid, k) in chunk {
            q = q.bind(eid).bind(k);
        }
        q.execute(&mut **tx)
            .await
            .context("bulk insert email_keywords")?;
    }
    Ok(())
}

async fn bulk_insert_email_attachments(
    tx: &mut Transaction<'_, Sqlite>,
    rows: &[EmailRow],
) -> Result<()> {
    for chunk in rows.chunks(SQL_CHUNK) {
        let mut sql = String::from("DELETE FROM email_attachments WHERE email_id IN (");
        push_placeholder_list(&mut sql, chunk.len());
        sql.push(')');
        let mut q = sqlx::query(&sql);
        for r in chunk {
            q = q.bind(&r.id);
        }
        q.execute(&mut **tx)
            .await
            .context("bulk delete email_attachments")?;
    }
    struct AttachBind<'a> {
        email_id: &'a str,
        part_id: &'a str,
        blob_id: &'a str,
        name: Option<&'a str>,
        ctype: Option<&'a str>,
        size: Option<i64>,
        disposition: Option<&'a str>,
        cid: Option<&'a str>,
    }
    let flat: Vec<AttachBind> = rows
        .iter()
        .flat_map(|r| {
            r.attachments.iter().map(move |a| AttachBind {
                email_id: &r.id,
                part_id: &a.part_id,
                blob_id: &a.blob_id,
                name: a.name.as_deref(),
                ctype: a.content_type.as_deref(),
                size: a.size,
                disposition: a.disposition.as_deref(),
                cid: a.cid.as_deref(),
            })
        })
        .collect();
    for chunk in flat.chunks(SQL_CHUNK) {
        let mut sql = String::from(
            "INSERT INTO email_attachments
                (email_id, part_id, blob_id, name, type, size, disposition, cid)
             VALUES ",
        );
        push_placeholders(&mut sql, chunk.len(), 8);
        sql.push_str(
            " ON CONFLICT(email_id, part_id) DO UPDATE SET
                blob_id = excluded.blob_id,
                name = excluded.name,
                type = excluded.type,
                size = excluded.size,
                disposition = excluded.disposition,
                cid = excluded.cid",
        );
        let mut q = sqlx::query(&sql);
        for a in chunk {
            q = q
                .bind(a.email_id)
                .bind(a.part_id)
                .bind(a.blob_id)
                .bind(a.name)
                .bind(a.ctype)
                .bind(a.size)
                .bind(a.disposition)
                .bind(a.cid);
        }
        q.execute(&mut **tx)
            .await
            .context("bulk insert email_attachments")?;
    }
    Ok(())
}

async fn bulk_insert_blob_refs(
    tx: &mut Transaction<'_, Sqlite>,
    refs: &[PendingCas],
) -> Result<()> {
    for chunk in refs.chunks(SQL_CHUNK) {
        let mut sql = String::from(
            "INSERT INTO blob_refs
                (id, kind, owning_id, slot, upstream_uuid, upstream_name, source_url,
                 content_type, blake3)
             VALUES ",
        );
        push_placeholders(&mut sql, chunk.len(), 9);
        // Existing refs win: matches `INSERT OR IGNORE` semantics
        // store_bytes used to provide.
        sql.push_str(" ON CONFLICT(id) DO NOTHING");
        let mut q = sqlx::query(&sql);
        for r in chunk {
            q = q
                .bind(&r.ref_id)
                .bind(r.kind)
                .bind(&r.owning_id)
                .bind(&r.slot)
                .bind(&r.ref_id) // upstream_uuid mirrors ref_id for mbox-derived refs.
                .bind(r.upstream_name.as_deref())
                .bind::<Option<&str>>(None) // source_url
                .bind(r.content_type.as_deref())
                .bind(&r.blake3);
        }
        q.execute(&mut **tx)
            .await
            .context("bulk insert blob_refs")?;
    }
    Ok(())
}

async fn bulk_insert_blob_refs_bookkeeping(
    tx: &mut Transaction<'_, Sqlite>,
    refs: &[PendingCas],
    now: &str,
) -> Result<()> {
    bulk_insert_bookkeeping_for_ids(tx, "blob_refs", refs.iter().map(|r| r.ref_id.as_str()), now)
        .await
}

async fn bulk_insert_cas_objects(
    tx: &mut Transaction<'_, Sqlite>,
    rows: &[PendingCas],
    now: &str,
) -> Result<()> {
    for chunk in rows.chunks(SQL_CHUNK) {
        let mut sql = String::from(
            "INSERT OR IGNORE INTO cas_objects \
             (blake3, byte_len, content_type, bytes, first_seen_at) VALUES ",
        );
        push_placeholders(&mut sql, chunk.len(), 5);
        let mut q = sqlx::query(&sql);
        for r in chunk {
            q = q
                .bind(&r.blake3)
                .bind(r.bytes.len() as i64)
                .bind(r.content_type.as_deref())
                .bind(&r.bytes[..])
                .bind(now);
        }
        q.execute(&mut **tx)
            .await
            .context("bulk insert cas_objects")?;
    }
    Ok(())
}

async fn bulk_insert_mailboxes(
    tx: &mut Transaction<'_, Sqlite>,
    account_id: &str,
    specs: &[(String, String, Option<&'static str>, String)],
) -> Result<()> {
    if specs.is_empty() {
        return Ok(());
    }
    let cols = 5;
    for chunk in specs.chunks(SQL_CHUNK) {
        let mut sql =
            String::from("INSERT INTO mailboxes (id, account_id, name, role, payload) VALUES ");
        push_placeholders(&mut sql, chunk.len(), cols);
        sql.push_str(
            " ON CONFLICT(id) DO UPDATE SET
                account_id = excluded.account_id,
                name = COALESCE(excluded.name, mailboxes.name),
                role = COALESCE(excluded.role, mailboxes.role),
                payload = jsonb(excluded.payload)",
        );
        let mut q = sqlx::query(&sql);
        for (id, name, role, payload) in chunk {
            q = q
                .bind(id)
                .bind(account_id)
                .bind(name)
                .bind(*role)
                .bind(payload);
        }
        q.execute(&mut **tx)
            .await
            .context("bulk insert mailboxes")?;
    }
    Ok(())
}

async fn bulk_insert_threads(
    tx: &mut Transaction<'_, Sqlite>,
    account_id: &str,
    specs: &[(String, i64, String)],
) -> Result<()> {
    if specs.is_empty() {
        return Ok(());
    }
    for chunk in specs.chunks(SQL_CHUNK) {
        let mut sql =
            String::from("INSERT INTO threads (id, account_id, email_count, payload) VALUES ");
        push_placeholders(&mut sql, chunk.len(), 4);
        sql.push_str(
            " ON CONFLICT(id) DO UPDATE SET
                account_id = excluded.account_id,
                email_count = excluded.email_count,
                payload = jsonb(excluded.payload)",
        );
        let mut q = sqlx::query(&sql);
        for (id, count, payload) in chunk {
            q = q.bind(id).bind(account_id).bind(*count).bind(payload);
        }
        q.execute(&mut **tx).await.context("bulk insert threads")?;
    }
    Ok(())
}

async fn bulk_insert_bookkeeping_for_ids<'a, I>(
    tx: &mut Transaction<'_, Sqlite>,
    table: &str,
    ids: I,
    now: &str,
) -> Result<()>
where
    I: IntoIterator<Item = &'a str>,
{
    let ids: Vec<&str> = ids.into_iter().collect();
    if ids.is_empty() {
        return Ok(());
    }
    let bk_table = format!("{table}_bookkeeping");
    for chunk in ids.chunks(SQL_CHUNK) {
        let mut sql = format!(
            "INSERT INTO {bk_table} (id, fetched_at, attempt_count, last_attempt_at, last_error) VALUES "
        );
        push_placeholders(&mut sql, chunk.len(), 5);
        sql.push_str(&format!(
            " ON CONFLICT(id) DO UPDATE SET
                fetched_at = excluded.fetched_at,
                attempt_count = {bk_table}.attempt_count + 1,
                last_attempt_at = excluded.last_attempt_at,
                last_error = NULL"
        ));
        let mut q = sqlx::query(&sql);
        for id in chunk {
            q = q
                .bind(*id)
                .bind(now)
                .bind(1_i64)
                .bind(now)
                .bind::<Option<&str>>(None);
        }
        q.execute(&mut **tx)
            .await
            .with_context(|| format!("bulk insert {bk_table}"))?;
    }
    Ok(())
}

/// Push `count` copies of `(?, ?, …)` separated by commas. Each tuple
/// has `cols` placeholders.
fn push_placeholders(sql: &mut String, count: usize, cols: usize) {
    for i in 0..count {
        if i > 0 {
            sql.push(',');
        }
        sql.push('(');
        for j in 0..cols {
            if j > 0 {
                sql.push(',');
            }
            sql.push('?');
        }
        sql.push(')');
    }
}

/// Push `count` comma-separated `?` placeholders (no surrounding
/// parens). Used for `WHERE id IN (?, ?, …)` lists.
fn push_placeholder_list(sql: &mut String, count: usize) {
    for i in 0..count {
        if i > 0 {
            sql.push(',');
        }
        sql.push('?');
    }
}

// ─────────────────────────────────────────────────────────────────────
// Label mapping
// ─────────────────────────────────────────────────────────────────────

enum LabelMap {
    Mailbox { role: Option<&'static str> },
    Keyword(&'static str),
    Unread,
    Drop,
}

fn map_label(label: &str) -> LabelMap {
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

/// Split an `X-Gmail-Labels` header. Labels are comma-separated;
/// commas inside a label are backslash-escaped (`\,`).
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
    let mut out = String::with_capacity(28);
    out.push_str("mbox-");
    for b in digest.iter().take(12) {
        out.push_str(&format!("{:02x}", b));
    }
    out
}

// ─────────────────────────────────────────────────────────────────────
// mail-parser helpers
// ─────────────────────────────────────────────────────────────────────

fn strip_angle(s: &str) -> &str {
    let t = s.trim();
    let t = t.strip_prefix('<').unwrap_or(t);
    t.strip_suffix('>').unwrap_or(t)
}

fn header_text(hv: &HeaderValue) -> Option<String> {
    match hv {
        HeaderValue::Text(s) => Some(s.to_string()),
        HeaderValue::TextList(list) => Some(list.join(", ")),
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

/// Walk every MIME part the parser surfaces as an attachment or inline
/// non-body part, yielding `(dotted_part_id, &MessagePart)`. Mirrors
/// the JMAP server's `partId` convention (1-based dotted paths).
fn iter_attachments<'a>(
    msg: &'a mail_parser::Message<'a>,
) -> impl Iterator<Item = (String, &'a mail_parser::MessagePart<'a>)> + 'a {
    let body_idx: std::collections::HashSet<usize> = msg
        .text_body
        .iter()
        .copied()
        .chain(msg.html_body.iter().copied())
        .collect();
    msg.attachments
        .iter()
        .copied()
        .chain(msg.html_body.iter().copied())
        .scan(std::collections::HashSet::new(), move |seen, idx| {
            if !seen.insert(idx) {
                return Some(None);
            }
            if body_idx.contains(&idx) {
                let part = msg.part(idx)?;
                if part.content_id().is_some() {
                    return Some(Some((idx, part)));
                }
                return Some(None);
            }
            let part = msg.part(idx)?;
            // Skip non-body text/html parts that mail-parser
            // sometimes surfaces in `attachments` (e.g. an alternate
            // body). They're not attachments in the JMAP sense.
            if matches!(part.body, PartType::Text(_) | PartType::Html(_))
                && part.content_id().is_none()
                && part.attachment_name().is_none()
            {
                return Some(None);
            }
            Some(Some((idx, part)))
        })
        .flatten()
        .map(|(idx, part)| (format!("{}", idx + 1), part))
}

// ─────────────────────────────────────────────────────────────────────
// Path + hash helpers
// ─────────────────────────────────────────────────────────────────────

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

fn collect_mbox_files(input_path: &Path) -> Result<Vec<PathBuf>> {
    let mut out: Vec<PathBuf> = Vec::new();
    if input_path.is_file() {
        out.push(input_path.to_path_buf());
    } else if input_path.is_dir() {
        walk_dir(input_path, &mut out)?;
    }
    out.sort();
    Ok(out)
}

fn walk_dir(dir: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("read_dir {}", dir.display()))? {
        let entry = entry.with_context(|| format!("entry in {}", dir.display()))?;
        let path = entry.path();
        if path.is_dir() {
            walk_dir(&path, out)?;
        } else if path.extension().and_then(|s| s.to_str()) == Some("mbox") {
            out.push(path);
        }
    }
    Ok(())
}

/// True iff the user-pointed input is an `.mbox` file or a directory
/// containing one. Sync's extract dispatch uses this to pick between
/// the JMAP API and the mbox extractors when a `SourceConfig::Email`
/// has no `sync:` block.
pub fn is_mbox_input(input_path: &Path) -> bool {
    if input_path.is_file() {
        return input_path.extension().and_then(|s| s.to_str()) == Some("mbox");
    }
    if input_path.is_dir() {
        let mut paths: Vec<PathBuf> = Vec::new();
        if walk_dir(input_path, &mut paths).is_ok() {
            return !paths.is_empty();
        }
    }
    false
}

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

    fn write_tmp_mbox(body: &str) -> (tempfile::TempDir, PathBuf) {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("trek.mbox");
        std::fs::write(&path, body).unwrap();
        (d, path)
    }

    #[test]
    fn streaming_iter_yields_each_message() {
        let (_d, path) = write_tmp_mbox(TWO_MSG_MBOX);
        let msgs: Vec<Vec<u8>> = iter_mbox_messages(&path)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0].starts_with(b"X-GM-THRID:"));
        assert!(msgs[1].starts_with(b"X-GM-THRID:"));
    }

    #[test]
    fn unescape_strips_one_gt_from_quoted_from_lines() {
        let body =
            "From 1@x Wed Jun 03 22:30:48 +0000 2026\nSubject: t\n\n>From the desk of...\nbody\n";
        let (_d, path) = write_tmp_mbox(body);
        let msgs: Vec<Vec<u8>> = iter_mbox_messages(&path)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(msgs.len(), 1);
        let s = std::str::from_utf8(&msgs[0]).unwrap();
        assert!(s.contains("From the desk"));
        assert!(!s.contains(">From the desk"));
    }

    #[test]
    fn split_gmail_labels_unescapes_commas() {
        let labels = split_gmail_labels(r"Inbox,Personal\, Custom,Starred");
        assert_eq!(labels, vec!["Inbox", "Personal, Custom", "Starred"]);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn end_to_end_lands_envelope_and_eml_blob() {
        let (_d, path) = write_tmp_mbox(TWO_MSG_MBOX);
        let work = tempfile::tempdir().unwrap();
        let db_path = work.path().join("e.doltlite_db");
        let db = RawDb::open(&db_path).await.unwrap();
        let pool = db.pool().clone();
        let summary = fetch(FetchOptions {
            db_path: db_path.clone(),
            db: Some(db),
            input_path: path,
            ..Default::default()
        })
        .await
        .unwrap();
        // Close the writer pool before re-opening — doltlite has one
        // writer per file; without an explicit close the second open
        // races the writes-in-flight and sees an empty working tree.
        pool.close().await;
        assert_eq!(summary.emails_upserted, 2);
        assert_eq!(summary.threads_upserted, 1);
        assert!(summary.mailboxes_upserted >= 2); // Inbox + Sent
        assert_eq!(summary.blobs_stored, 2); // two .eml blobs, no attachments

        let db = RawDb::open(&db_path).await.unwrap();
        let emails = db.load_emails().await.unwrap();
        assert_eq!(emails.len(), 2);
        let picard = emails
            .iter()
            .find(|e| e.subject.as_deref() == Some("Make it so"))
            .unwrap();
        assert_eq!(picard.id, "msg-one@enterprise.starfleet");
        assert_eq!(picard.thread_id, "1111");
        // .eml is in CAS keyed by emails.blob_id.
        assert!(db.blob_exists(&picard.blob_id).await.unwrap());
        // Unread label suppressed $seen for Picard's message; Riker
        // (no Unread) gets $seen.
        let joins = db.load_email_joins().await.unwrap();
        assert!(!joins.keywords[&picard.id].iter().any(|k| k == "$seen"));
        assert!(joins.keywords[&picard.id].iter().any(|k| k == "$flagged"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn re_running_is_idempotent() {
        let (_d, path) = write_tmp_mbox(TWO_MSG_MBOX);
        let work = tempfile::tempdir().unwrap();
        let db_path = work.path().join("e.doltlite_db");
        for _ in 0..2 {
            let db = RawDb::open(&db_path).await.unwrap();
            let pool = db.pool().clone();
            fetch(FetchOptions {
                db_path: db_path.clone(),
                db: Some(db),
                input_path: path.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
            pool.close().await;
        }
        let db = RawDb::open(&db_path).await.unwrap();
        assert_eq!(db.load_emails().await.unwrap().len(), 2);
    }
}
