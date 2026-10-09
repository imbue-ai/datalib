//! mbox extractor. Walks a Google Takeout `.mbox` file (RFC 4155 mboxrd
//! framing; Gmail's message id on each `From ` line, plus its
//! `X-GM-THRID` / `X-Gmail-Labels` headers) and lands every
//! message into the shared email raw store as if it had come off a JMAP
//! server. No body parsing here — render handles that off the `.eml` blob.
//!
//! Every run reads every `.mbox` under the input. The folder is the unit:
//! two files can hold one message and rows are keyed by content, so only a
//! clean read of all of them says what left the input.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use datalib_etl::blob_cas::{blake3_hex, CasEdgeAccumulator, CasEdgeRow as _};
use datalib_etl::bulk::{bulk_upsert_entity_in_tx, push_placeholder_list, SQL_CHUNK};
use datalib_etl::control::DownloadControl;
use datalib_etl::download_problems::{self, RunProblem, SkippedRecord};
use datalib_etl::progress::Progress;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl_files::fswalk;
use mail_parser::MessageParser;
use serde::Serialize;
use sqlx::{Sqlite, Transaction};

use super::db::{EmailRow, RawDb};
use super::envelope::{self, header_text, GmailId};
use super::labels::{self, mailbox_id, map_label, split_gmail_labels, LabelMap};
use super::schema_raw::{AccountRow, EmailKeywordRow, EmailMailboxRow, EmlBlobRow};

/// Maximum emails accumulated in memory before we flush a bulk batch
/// to disk. Keeps peak RSS bounded while still amortizing doltlite's
/// per-transaction page rewrite across many rows.
const FLUSH_BATCH: usize = 2000;

/// Account-row data from the source's `mbox` block.
#[derive(Debug, Clone, Default)]
pub struct MboxAccountConfig {
    /// The account every email is filed under; with none, one is made
    /// from the input's name.
    pub account_id: Option<String>,
    pub display_name: Option<String>,
    pub email_address: Option<String>,
    pub is_personal: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// The store this run writes into, opened and closed by the caller.
    /// A download never opens a store of its own: one writer per file
    /// (`datalib/backend/etl/README.md` § "One writer per file, by
    /// construction").
    pub db: RawDb,
    /// `.mbox` file (or directory containing `*.mbox` files).
    pub input_path: PathBuf,
    pub account_config: MboxAccountConfig,
    /// When non-empty, only ingest messages carrying an `X-Gmail-Labels` label
    /// whose full path exactly matches one of these. Gmail nested labels are
    /// already stored as `Parent/Child`, so this is a direct string compare.
    pub only_labels: Vec<String>,
    pub progress: Progress,
    pub control: DownloadControl,
}

impl FetchOptions {
    /// Every field defaulted except the store, which the caller opens and
    /// closes.
    pub fn new(db: RawDb) -> Self {
        Self {
            db,
            input_path: PathBuf::new(),
            account_config: MboxAccountConfig::default(),
            only_labels: Vec::new(),
            progress: Progress::noop(),
            control: DownloadControl::default(),
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
    pub parse_errors: usize,
    /// Emails deleted because no mbox file holds them.
    pub emails_removed: usize,
    /// Labels no message in the files carries any more, whose rows went.
    pub mailboxes_removed: usize,
}

/// The config key `only_labels` comes from, for its `config:` rows.
const K_ONLY_LABELS: &str = "only_extract_labels";

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let (pool, stop) = (opts.db.pool().clone(), opts.control.stop.clone());
    run_problems::collecting(&pool, &stop, |found| read_files(opts, found)).await
}

async fn read_files(opts: FetchOptions, found: RunProblems) -> Result<FetchSummary> {
    let db = opts.db.clone();
    let (files, walk_errors) = list_mbox_files(&opts.input_path)?;
    found.extend(walk_problem(&opts.input_path, &walk_errors));
    let account_id = opts
        .account_config
        .account_id
        .clone()
        .unwrap_or_else(|| default_account_id(&opts.input_path));

    // An `.eml` the store already holds is not written again: rewriting
    // its edge would restamp it on every run.
    let known_blobs = db.loaded_blob_ids().await?;

    // The bar runs over bytes consumed rather than emails processed, because
    // the filesystem gives a real total up front and an email count only
    // resolves at EOF.
    opts.progress
        .set_length(Some(files.iter().map(|f| f.size).sum()));

    let label_filter: Option<HashSet<String>> = if opts.only_labels.is_empty() {
        None
    } else {
        Some(
            opts.only_labels
                .iter()
                .map(|s| s.trim().to_string())
                .collect(),
        )
    };
    // The label rows already here, by name. A row a Gmail API sync made
    // carries Google's real label id, and a message filed under that name
    // lands there rather than on a second, name-keyed row.
    let held_mailboxes = db.mailbox_names(&account_id).await?;
    let gmail_prefix = labels::gmail_mailbox_prefix(&account_id);
    let real_ids: HashMap<String, String> = held_mailboxes
        .iter()
        .filter(|(id, _)| id.starts_with(&gmail_prefix))
        .map(|(id, name)| (name.clone(), id.clone()))
        .collect();
    let unfiltered = label_filter.is_none();
    let mut accumulator = Accumulator::new(account_id.clone(), label_filter, real_ids);
    let mut summary = FetchSummary::default();
    let mut batch = PendingBatch::default();
    let mut emails_seen: u64 = 0;
    let mut unparsed_files: Vec<SkippedRecord> = Vec::new();

    for f in &files {
        let messages = match iter_mbox_messages(&f.path) {
            Ok(messages) => messages,
            Err(e) => {
                summary.parse_errors += 1;
                found.push(file_unread(f, &e));
                continue;
            }
        };
        let mut unparsed = Unparsed::default();
        let mut failed: Option<anyhow::Error> = None;
        let mut messages_in_file = 0usize;
        for message in messages {
            // An error can repeat on every read, so the file ends here.
            let message = match message {
                Ok(m) => m,
                Err(e) => {
                    failed = Some(e);
                    break;
                }
            };
            messages_in_file += 1;
            opts.progress.inc(message.bytes_consumed);
            match accumulator.ingest_message(
                &message.raw,
                message.gmail_id,
                &known_blobs,
                &mut batch,
                &mut summary,
            ) {
                Ok(true) => {
                    emails_seen += 1;
                    opts.progress.set_message(&format!("{emails_seen} emails"));
                    if batch.emails.len() >= FLUSH_BATCH {
                        flush_batch(&db, &mut batch, &mut summary).await?;
                    }
                }
                Ok(false) => {} // duplicate, or outside the label filter
                Err(e) => {
                    summary.parse_errors += 1;
                    unparsed.add(&e);
                }
            }
        }
        unparsed_files.extend(unparsed.problem(f));
        // An mbox has no envelope that could say "no messages", so a file
        // with none (0 bytes, no `From ` line) is not read as an empty
        // mailbox: it holds the prune back, and deleting the file is what
        // drops what it held.
        let failed = failed.or_else(|| {
            (messages_in_file == 0)
                .then(|| anyhow!("the file holds no message: not one `From ` line"))
        });
        if let Some(e) = failed {
            summary.parse_errors += 1;
            found.push(file_unread(f, &e));
        }
    }
    flush_batch(&db, &mut batch, &mut summary).await?;
    flush_account_and_lookups(
        &db,
        &account_id,
        &opts.account_config,
        &accumulator,
        &mut summary,
    )
    .await?;
    found.skipped("mbox", unparsed_files);

    // Every message of every file was read: the run knows what the input
    // holds, so it can say what left it and which configured label
    // nothing carries.
    let seen = &accumulator.seen_email_ids;
    if walk_errors.is_empty() && summary.parse_errors == 0 {
        // `seen` is every message id the read met, before the label
        // filter, so narrowing `only_labels` never deletes.
        summary.emails_removed = db.prune_emails_to(&account_id, seen).await?;
        for was in other_accounts(&db, &account_id).await? {
            summary.emails_removed += retire_account(&db, &was).await?;
        }
        // Every message was read and refiled, so a label this run's
        // own recipe minted that none of them carries is gone from the
        // export. A label filter hides the rest of the labels, and a
        // real Gmail id is the API sync's to retire.
        if unfiltered {
            let minted: BTreeSet<&str> = accumulator
                .mailboxes
                .values()
                .map(|m| m.id.as_str())
                .collect();
            let gone: Vec<(String, Option<String>)> = held_mailboxes
                .keys()
                .filter(|id| id.starts_with(labels::NAME_KEYED_PREFIX))
                .filter(|id| !minted.contains(id.as_str()))
                .map(|id| (id.clone(), None))
                .collect();
            summary.mailboxes_removed = gone.len();
            let now = datalib_time::IsoOffsetTimestamp::now_local();
            super::refile_mailboxes(&db, &now, &gone).await?;
        }
        found.config(unmatched_labels(
            &opts.only_labels,
            &accumulator.labels_seen,
        ));
    } else {
        let held_back = stored_but_unseen(&db, &account_id, seen).await?;
        if held_back > 0 {
            found.push(RunProblem::listing(
                "removed_records",
                format!(
                    "{held_back} stored emails are in no file this run read, but not every \
                     file read cleanly, so none was deleted"
                ),
            ));
        }
    }

    Ok(summary)
}

/// One `.mbox` to read, named by its path under the input.
struct MboxFile {
    path: PathBuf,
    rel: String,
    size: u64,
}

fn is_mbox(path: &Path) -> bool {
    path.extension().and_then(|s| s.to_str()) == Some("mbox")
}

/// Every `.mbox` under `input`, in name order, and the entries the walk
/// could not see. A file as the input is that one file.
fn list_mbox_files(input: &Path) -> Result<(Vec<MboxFile>, Vec<fswalk::WalkError>)> {
    let root = input
        .canonicalize()
        .with_context(|| format!("resolve {}", input.display()))?;
    if root.is_file() {
        let size = std::fs::metadata(&root)
            .with_context(|| format!("stat {}", root.display()))?
            .len();
        let rel = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let files = is_mbox(&root).then_some(MboxFile {
            path: root,
            rel,
            size,
        });
        return Ok((files.into_iter().collect(), Vec::new()));
    }
    let (walked, errors) = fswalk::walk_files(&root, &[], None, is_mbox)
        .with_context(|| format!("walk {}", root.display()))?;
    let mut files: Vec<MboxFile> = walked
        .into_iter()
        .map(|w| MboxFile {
            size: w.meta.len(),
            path: w.path,
            rel: w.rel,
        })
        .collect();
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok((files, errors))
}

/// The `listing:files` row of a walk that could not see every entry. A
/// file it missed may hold any stored email, so nothing is deleted.
fn walk_problem(input: &Path, errors: &[fswalk::WalkError]) -> Option<RunProblem> {
    let first = errors.first()?;
    Some(RunProblem::listing(
        "files",
        format!(
            "{}: {} ({} unreadable under {}; nothing was deleted this run)",
            first.path.display(),
            first.error,
            errors.len(),
            input.display(),
        ),
    ))
}

/// The accounts the store files rows under other than `account_id`: ids
/// the config named before. Once every file has been read under the one
/// it names now, what is left under them is no file's.
async fn other_accounts(db: &RawDb, account_id: &str) -> Result<Vec<String>> {
    sqlx::query_scalar(
        "SELECT account_id FROM emails WHERE account_id <> ?1 \
         UNION SELECT account_id FROM threads WHERE account_id <> ?1 \
         UNION SELECT account_id FROM mailboxes WHERE account_id <> ?1 \
         UNION SELECT id FROM accounts WHERE id <> ?1",
    )
    .bind(account_id)
    .fetch_all(db.pool())
    .await
    .context("list the accounts the config no longer names")
}

/// How many stored emails a clean read would have deleted: under another
/// account, or in no file this run read.
async fn stored_but_unseen(db: &RawDb, account_id: &str, seen: &BTreeSet<String>) -> Result<usize> {
    let stored: Vec<(String, String)> = sqlx::query_as("SELECT id, account_id FROM emails")
        .fetch_all(db.pool())
        .await
        .context("list the stored emails")?;
    Ok(stored
        .iter()
        .filter(|(id, account)| account != account_id || !seen.contains(id))
        .count())
}

/// Drop what is left under an account the config no longer names: its
/// emails (no file holds them), its threads and its name-keyed labels. The
/// account row goes once nothing names it; a label with a real Gmail id is
/// the API sync's.
async fn retire_account(db: &RawDb, account_id: &str) -> Result<usize> {
    let removed = db.prune_emails_to(account_id, &BTreeSet::new()).await?;
    let labels: Vec<(String, Option<String>)> = db
        .mailbox_names(account_id)
        .await?
        .into_keys()
        .filter(|id| id.starts_with(labels::NAME_KEYED_PREFIX))
        .map(|id| (id, None))
        .collect();
    let now = datalib_time::IsoOffsetTimestamp::now_local();
    super::refile_mailboxes(db, &now, &labels).await?;
    let still_named: i64 = sqlx::query_scalar(
        "SELECT (SELECT count(*) FROM emails WHERE account_id = ?1) \
              + (SELECT count(*) FROM threads WHERE account_id = ?1) \
              + (SELECT count(*) FROM mailboxes WHERE account_id = ?1)",
    )
    .bind(account_id)
    .fetch_one(db.pool())
    .await
    .context("count what still names the old account")?;
    if still_named == 0 {
        datalib_etl::prune::prune_scope(
            db.pool(),
            "accounts",
            &[("id", account_id)],
            &HashSet::new(),
        )
        .await?;
    }
    Ok(removed)
}

/// The messages of one file that would not parse.
#[derive(Default)]
struct Unparsed {
    count: usize,
    first: Option<String>,
}

impl Unparsed {
    fn add(&mut self, e: &anyhow::Error) {
        self.count += 1;
        self.first.get_or_insert_with(|| format!("{e:#}"));
    }

    fn problem(&self, f: &MboxFile) -> Option<SkippedRecord> {
        let first = self.first.as_deref()?;
        Some(SkippedRecord {
            entry: f.rel.clone(),
            problem: datalib_problems::Problem::record(
                datalib_problems::Reason::Undeserializable,
                &format!(
                    "{}: {} messages could not be parsed; first: {first}",
                    f.rel, self.count
                ),
            ),
        })
    }
}

/// A file the run could not read through, as a `listing:` row.
fn file_unread(f: &MboxFile, e: &anyhow::Error) -> RunProblem {
    RunProblem::listing(&format!("mbox {}", f.rel), format!("{e:#}"))
}

/// The configured labels no message in the files carries.
fn unmatched_labels(
    configured: &[String],
    seen: &BTreeSet<String>,
) -> Vec<download_problems::DownloadProblem> {
    configured
        .iter()
        .map(|l| l.trim())
        .filter(|l| !seen.contains(*l))
        .map(|l| {
            download_problems::DownloadProblem::not_found(
                K_ONLY_LABELS,
                l,
                "no message in the mbox files carries this label",
            )
        })
        .collect()
}

// ─────────────────────────────────────────────────────────────────────
// Streaming mbox iterator

struct MboxMessage {
    raw: Vec<u8>,
    /// From the envelope `From ` line, when it is Takeout's.
    gmail_id: Option<GmailId>,
    /// mbox bytes read since the previous message, for a byte-keyed
    /// progress bar.
    bytes_consumed: u64,
}

/// Iterate `path` yielding one RFC 5322 message at a time, ending after
/// the first read error. Envelope `From ` lines are stripped and `>From `
/// escapes unquoted. Streams via `BufReader`, so peak RSS stays bounded.
fn iter_mbox_messages(path: &Path) -> Result<impl Iterator<Item = Result<MboxMessage>>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::with_capacity(1 << 16, file);
    let mut pending: Option<(Vec<u8>, Option<GmailId>)> = None;
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut started = false;
    // Accumulates every byte read from the file and resets at each yield, so
    // the caller's increments sum to the file's `metadata().len()` however many
    // emails it held.
    let mut bytes_since_yield: u64 = 0;
    let it = std::iter::from_fn(move || loop {
        buf.clear();
        let n = match reader.read_until(b'\n', &mut buf) {
            Ok(0) => {
                // EOF; flush any pending message together with the
                // remaining bytes counted on this last `read_until`
                // (which returned 0 — nothing to add).
                let bytes_consumed = std::mem::take(&mut bytes_since_yield);
                return pending.take().map(|(raw, gmail_id)| {
                    Ok(MboxMessage {
                        raw,
                        gmail_id,
                        bytes_consumed,
                    })
                });
            }
            Ok(n) => n,
            Err(e) => return Some(Err(e.into())),
        };
        bytes_since_yield += n as u64;
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
            pending = Some((Vec::with_capacity(4096), takeout_gmail_id(line)));
            started = true;
            if let Some((raw, gmail_id)) = prev {
                let bytes_consumed = std::mem::take(&mut bytes_since_yield);
                return Some(Ok(MboxMessage {
                    raw,
                    gmail_id,
                    bytes_consumed,
                }));
            }
            continue;
        }
        if !started {
            // Tolerate leading junk before the first `From ` line.
            continue;
        }
        let (target, _) = pending.as_mut().expect("started => Some");
        let unescaped = unescape_from_line(line);
        target.extend_from_slice(&unescaped);
        target.push(b'\n');
    });
    // A read error can come back on every read after it, so the stream
    // ends at the first.
    Ok(it.scan(false, |failed, item| {
        if *failed {
            return None;
        }
        *failed = item.is_err();
        Some(item)
    }))
}

fn is_from_line(line: &[u8]) -> bool {
    line.len() >= 5 && &line[..5] == b"From "
}

/// Takeout writes Gmail's message id, in decimal, as the sender of each
/// `From ` line: `From 1853466712473707184@xxx Mon Jan 05 …`. Any other
/// mbox names a real sender there.
fn takeout_gmail_id(from_line: &[u8]) -> Option<GmailId> {
    let sender = from_line.get(5..)?.split(|b| *b == b' ').next()?;
    let digits = sender.strip_suffix(b"@xxx")?;
    if digits.is_empty() || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    GmailId::from_takeout(std::str::from_utf8(digits).ok()?)
}

fn unescape_from_line(line: &[u8]) -> Vec<u8> {
    let n = line.iter().take_while(|b| **b == b'>').count();
    if n >= 1 && line.len() >= n + 5 && &line[n..n + 5] == b"From " {
        line[1..].to_vec()
    } else {
        line.to_vec()
    }
}

// Per-message envelope extraction

struct Accumulator {
    account_id: String,
    mailboxes: BTreeMap<String, MailboxEntry>,
    /// Label name → the real Gmail id a row already carries for it.
    real_ids: HashMap<String, String>,
    threads: BTreeMap<String, Vec<ThreadMember>>,
    seen_email_ids: BTreeSet<String>,
    /// Every label any message carried, before the label filter.
    labels_seen: BTreeSet<String>,
    /// When `Some`, only messages carrying a label whose full path is
    /// in this set are ingested (the rest are dropped before any row or
    /// blob lands). `None` = ingest everything. See
    /// [`FetchOptions::only_labels`].
    label_filter: Option<HashSet<String>>,
}

struct MailboxEntry {
    id: String,
    role: Option<&'static str>,
    /// The row is the Gmail API sync's, and stays as it wrote it.
    borrowed: bool,
}

#[derive(Clone)]
struct ThreadMember {
    id: String,
    received: String,
}

impl Accumulator {
    fn new(
        account_id: String,
        label_filter: Option<HashSet<String>>,
        real_ids: HashMap<String, String>,
    ) -> Self {
        Self {
            account_id,
            mailboxes: BTreeMap::new(),
            real_ids,
            threads: BTreeMap::new(),
            seen_email_ids: BTreeSet::new(),
            labels_seen: BTreeSet::new(),
            label_filter,
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
        gmail_id: Option<GmailId>,
        known_blobs: &std::collections::HashMap<String, String>,
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
        let email_id = envelope::email_id(gmail_id, &msg, &eml_blob_id);
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
        self.labels_seen
            .extend(labels.iter().map(|l| l.trim().to_string()));

        // Label filter: drop the message before any row/blob/thread
        // bookkeeping if none of its labels is in the allow-set. Matched
        // on the raw label string (Gmail nested labels are already
        // `Parent/Child` paths), trimmed to mirror the JMAP resolver.
        if let Some(allow) = &self.label_filter {
            if !labels.iter().any(|l| allow.contains(l.trim())) {
                return Ok(false);
            }
        }

        let (mailbox_ids, keywords) = self.resolve_labels(&labels);

        // Date — load-bearing for thread ordering, so computed up front.
        let received_at = envelope::received_at(&msg);

        // Queue the .eml itself (the canonical body — everything we
        // need for render lives inside it) into the shared CAS-edge
        // accumulator. It carries the bytes through to the
        // end-of-batch `put_many` + `email_blobs` edge upsert; the
        // edge's `blake3` comes straight off the accumulated bytes.
        if known_blobs.contains_key(&eml_blob_id) || pending.seen_blob_ids.contains(&eml_blob_id) {
            summary.blobs_skipped += 1;
        } else {
            pending.seen_blob_ids.insert(eml_blob_id.clone());
            pending.cas.add_fetched(
                &email_id,
                &eml_blob_id,
                raw.to_vec(),
                Some("message/rfc822".to_string()),
            );
            summary.blobs_stored += 1;
        }

        self.threads
            .entry(thread_id.clone())
            .or_default()
            .push(ThreadMember {
                id: email_id.clone(),
                received: received_at.clone().unwrap_or_default(),
            });

        // Synthesize a JMAP-shaped `Email/get` envelope so the row goes
        // through the exact same `EmailRow::from_jmap_envelope` path as
        // the JMAP source. Shared with every other non-JMAP mode — see
        // `super::envelope`.
        let envelope = envelope::synthesize(
            raw,
            &msg,
            &envelope::TransportFacts {
                email_id: email_id.clone(),
                blob_id: eml_blob_id.clone(),
                thread_id: thread_id.clone(),
                mailbox_ids,
                keywords,
            },
        );

        if let Some(row) = EmailRow::from_jmap_envelope(&self.account_id, &envelope) {
            pending.emails.push(row);
        }
        Ok(true)
    }

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
        let (id, borrowed) = match self.real_ids.get(name) {
            Some(real) => (real.clone(), true),
            None => (mailbox_id(&self.account_id, name), false),
        };
        self.mailboxes.insert(
            name.to_string(),
            MailboxEntry {
                id: id.clone(),
                role,
                borrowed,
            },
        );
        id
    }
}

// Bulk-write flush path

/// Everything the next flush will hand to doltlite. Accumulating in memory
/// and flushing as one entity-pool transaction plus one CAS-pool transaction
/// is dramatically cheaper than per-row writes: doltlite rewrites every page a
/// transaction touched at each `COMMIT` (docs/dev/doltlite.md § "What a write
/// costs").
#[derive(Default)]
struct PendingBatch {
    emails: Vec<EmailRow>,
    /// `.eml` bytes + their `email_blobs` edges for this batch. The
    /// accumulator holds the bytes (in its [`BlobBundle`]) and resolves
    /// each edge's `blake3` off them at flush time — see
    /// [`datalib_etl::blob_cas::CasEdgeAccumulator`].
    cas: CasEdgeAccumulator,
    /// In-run dedupe of blob ref ids. For mbox the ref_id is the
    /// `.eml`'s blake3, so identical bodies collapse to one row and
    /// doltlite never sees a conflicting bind pair inside one multi-row
    /// statement.
    seen_blob_ids: std::collections::HashSet<String>,
}

impl PendingBatch {
    fn clear(&mut self) {
        self.emails.clear();
        self.cas = CasEdgeAccumulator::new();
        // `seen_blob_ids` deliberately persists across flushes: an
        // identical attachment landing in a later batch should still
        // dedupe against an earlier flush in the same run.
    }
}

/// Flush one accumulated `PendingBatch` to disk: one entity-pool
/// transaction (emails + join tables + emails bookkeeping), then the
/// shared CAS-edge flush ([`CasEdgeAccumulator::flush`]) which does
/// the CAS `put_many` + `email_blobs` edge upsert + edge bookkeeping.
async fn flush_batch(
    db: &RawDb,
    batch: &mut PendingBatch,
    summary: &mut FetchSummary,
) -> Result<()> {
    if batch.emails.is_empty() {
        return Ok(());
    }

    let mut etx = db.pool().begin().await.context("begin entity tx")?;
    bulk_insert_emails(&mut etx, &batch.emails).await?;
    bulk_insert_email_mailboxes(&mut etx, &batch.emails).await?;
    bulk_insert_email_keywords(&mut etx, &batch.emails).await?;
    etx.commit().await.context("commit entity tx")?;

    // CAS bytes + `email_blobs` edges (each carrying the bundle-derived
    // blake3) + edge bookkeeping, all via the shared primitive.
    batch
        .cas
        .flush(db.pool(), db.cas(), |email_id, blob_id, blake3| {
            EmlBlobRow {
                id: EmlBlobRow::pk_recipe(email_id, blob_id),
                email_id: email_id.to_string(),
                blob_id: blob_id.to_string(),
                blake3: blake3.map(str::to_string),
            }
        })
        .await?;

    summary.emails_upserted += batch.emails.len();
    batch.clear();
    Ok(())
}

async fn flush_account_and_lookups(
    db: &RawDb,
    account_id: &str,
    account_config: &MboxAccountConfig,
    accumulator: &Accumulator,
    summary: &mut FetchSummary,
) -> Result<()> {
    let now = datalib_time::IsoOffsetTimestamp::now_local();
    let mut tx = db.pool().begin().await.context("begin lookups tx")?;

    // Account row: route through `AccountRow::from_mbox_config` and
    // the shared bulk upsert so the synthesized row has the
    // exact same shape (columns + JSONB payload) that the JMAP path
    // produces. Display name defaults to the account id when the
    // config doesn't supply one; `is_personal` defaults to true.
    let display_name = account_config
        .display_name
        .clone()
        .unwrap_or_else(|| account_id.to_string());
    let account_row = AccountRow::from_mbox_config(
        account_id,
        Some(display_name.as_str()),
        account_config.email_address.as_deref(),
        account_config.is_personal.unwrap_or(true),
    );
    datalib_etl::bulk::bulk_upsert_first_seen_in_tx(&mut tx, &[account_row], &now).await?;

    // Mailboxes.
    let mailbox_specs: Vec<(String, String, Option<&'static str>, String)> = accumulator
        .mailboxes
        .iter()
        .filter(|(_, entry)| !entry.borrowed)
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
    datalib_etl::bulk::bulk_stamp_first_seen(
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
    datalib_etl::bulk::bulk_stamp_first_seen(
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
    // `EmailRow` carries its own `BulkUpsertable` impl; the sidecar is
    // stamped once, since every run reads every file.
    let now = datalib_time::IsoOffsetTimestamp::now_local();
    datalib_etl::bulk::bulk_upsert_first_seen_in_tx(tx, rows, &now).await
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
        // Audited: static template; the only interpolation is a `?,?,?` run sized
        // from the chunk length. Every value is bound.
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for r in chunk {
            q = q.bind(r.id());
        }
        q.execute(&mut **tx)
            .await
            .context("bulk delete email_mailboxes")?;
    }
    let mut join_rows: Vec<EmailMailboxRow> = Vec::new();
    for r in rows {
        let id = r.id();
        for m in r.mailbox_ids() {
            join_rows.push(EmailMailboxRow::new(id, &m));
        }
    }
    bulk_upsert_entity_in_tx(tx, &join_rows)
        .await
        .context("bulk insert email_mailboxes")?;
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
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for r in chunk {
            q = q.bind(r.id());
        }
        q.execute(&mut **tx)
            .await
            .context("bulk delete email_keywords")?;
    }
    let mut join_rows: Vec<EmailKeywordRow> = Vec::new();
    for r in rows {
        let id = r.id();
        for k in r.keywords() {
            join_rows.push(EmailKeywordRow::new(id, &k));
        }
    }
    bulk_upsert_entity_in_tx(tx, &join_rows)
        .await
        .context("bulk insert email_keywords")?;
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
    for chunk in specs.chunks(SQL_CHUNK) {
        // `jsonb(?)` on the insert as on the update: a payload stored as
        // text once and as JSONB the next time differs by its bytes, and
        // an unchanged mailbox would read as modified.
        let mut sql =
            String::from("INSERT INTO mailboxes (id, account_id, name, role, payload) VALUES ");
        sql.push_str(&vec!["(?, ?, ?, ?, jsonb(?))"; chunk.len()].join(", "));
        sql.push_str(
            " ON CONFLICT(id) DO UPDATE SET
                account_id = excluded.account_id,
                name = COALESCE(excluded.name, mailboxes.name),
                role = COALESCE(excluded.role, mailboxes.role),
                payload = jsonb(excluded.payload)",
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
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
        sql.push_str(&vec!["(?, ?, ?, jsonb(?))"; chunk.len()].join(", "));
        sql.push_str(
            " ON CONFLICT(id) DO UPDATE SET
                account_id = excluded.account_id,
                email_count = excluded.email_count,
                payload = jsonb(excluded.payload)",
        );
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for (id, count, payload) in chunk {
            q = q.bind(id).bind(account_id).bind(*count).bind(payload);
        }
        q.execute(&mut **tx).await.context("bulk insert threads")?;
    }
    Ok(())
}

// Path + hash helpers

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

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_etl::store_handle::RawStoreHandle;

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
        let msgs: Vec<MboxMessage> = iter_mbox_messages(&path)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap();
        assert_eq!(msgs.len(), 2);
        assert!(msgs[0].raw.starts_with(b"X-GM-THRID:"));
        assert!(msgs[1].raw.starts_with(b"X-GM-THRID:"));
        assert_eq!(msgs[0].gmail_id, GmailId::from_takeout("1111"));
        assert_eq!(msgs[1].gmail_id, GmailId::from_takeout("2222"));
    }

    /// Only Takeout's `<digits>@xxx` is a Gmail id; any other mbox puts a
    /// real sender there, and reading one as an id would key rows by it.
    #[test]
    fn reads_a_gmail_id_only_from_a_takeout_from_line() {
        let id = |line: &str| takeout_gmail_id(line.as_bytes());
        assert_eq!(
            id("From 1853466712473707184@xxx Mon Jan 05 09:00:00 +0000 2026"),
            GmailId::from_api("19b8d627a801a2b0"),
        );
        assert_eq!(
            id("From picard@enterprise.starfleet Mon Jan 05 09:00:00 2026"),
            None
        );
        assert_eq!(
            id("From 1234@enterprise.starfleet Mon Jan 05 09:00:00 2026"),
            None
        );
        assert_eq!(id("From 12ab@xxx Mon Jan 05 09:00:00 2026"), None);
        assert_eq!(id("From @xxx Mon Jan 05 09:00:00 2026"), None);
        assert_eq!(id("From 99999999999999999999@xxx Mon Jan 05 2026"), None);
    }

    #[test]
    fn unescape_strips_one_gt_from_quoted_from_lines() {
        let body =
            "From 1@x Wed Jun 03 22:30:48 +0000 2026\nSubject: t\n\n>From the desk of...\nbody\n";
        let (_d, path) = write_tmp_mbox(body);
        let msgs: Vec<Vec<u8>> = iter_mbox_messages(&path)
            .unwrap()
            .collect::<Result<Vec<_>>>()
            .unwrap()
            .into_iter()
            .map(|m| m.raw)
            .collect();
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
        let summary = fetch(FetchOptions {
            input_path: path,
            ..FetchOptions::new(db.clone())
        })
        .await
        .unwrap();
        // Close before re-opening — doltlite has one writer per file;
        // without an explicit close the second open races the
        // writes-in-flight and sees an empty working tree.
        db.commit_all("test").await.unwrap();
        db.close().await;
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
        // Gmail's id from the `From ` line (1111 = 0x457), not the Message-Id.
        assert_eq!(picard.id, "0000000000000457");
        assert_eq!(picard.thread_id, "1111");
        // .eml is in CAS keyed by emails.blob_id. The path goes
        // emails.blob_id → email_blobs.blake3 → cas_objects.bytes.
        let blake3: Option<String> =
            sqlx::query_scalar("SELECT blake3 FROM email_blobs WHERE email_id = ?")
                .bind(&picard.id)
                .fetch_one(db.pool())
                .await
                .unwrap();
        let blake3 = blake3.expect("email_blobs.blake3 set by mbox flush");
        let exists: bool =
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM cas_objects WHERE blake3 = ?)")
                .bind(&blake3)
                .fetch_one(db.cas().pool())
                .await
                .unwrap();
        assert!(exists);
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
        let mut summaries: Vec<FetchSummary> = Vec::new();
        let mut commits: Vec<Option<String>> = Vec::new();
        for _ in 0..2 {
            let db = RawDb::open(&db_path).await.unwrap();
            let s = fetch(FetchOptions {
                input_path: path.clone(),
                ..FetchOptions::new(db.clone())
            })
            .await
            .unwrap();
            summaries.push(s);
            commits.push(
                datalib_etl::doltlite_raw::commit_run(db.pool(), "test")
                    .await
                    .unwrap(),
            );
            db.close().await;
        }
        assert!(commits[0].is_some());
        assert_eq!(
            commits[1], None,
            "reading an unchanged folder again changes nothing in the store"
        );
        let db = RawDb::open(&db_path).await.unwrap();
        assert_eq!(db.load_emails().await.unwrap().len(), 2);

        // The second run reads the file again and stores what it read
        // again; the `.eml` bytes are already in the CAS, so none is
        // written twice, and nothing is removed.
        assert_eq!(summaries[0].emails_upserted, 2);
        assert_eq!(summaries[1].emails_upserted, 2);
        assert_eq!(summaries[1].blobs_stored, 0);
        assert_eq!(summaries[1].blobs_skipped, 2);
        assert_eq!(summaries[1].emails_removed, 0);
    }

    /// `build` gets the store handle this run opens, and returns the
    /// options for it. The handle is closed here, so no two runs
    /// overlap on one file.
    async fn run_once(
        db_path: &Path,
        path: &Path,
        build: impl FnOnce(RawDb) -> FetchOptions,
    ) -> FetchSummary {
        let db = RawDb::open(db_path).await.unwrap();
        let s = fetch(FetchOptions {
            input_path: path.to_path_buf(),
            ..build(db.clone())
        })
        .await
        .unwrap();
        db.commit_all("test").await.unwrap();
        db.close().await;
        s
    }

    /// An unchanged file whose label filter widened lands the newly
    /// admitted messages: a skip keyed on the file's bytes alone would
    /// never read it again.
    #[tokio::test(flavor = "multi_thread")]
    async fn widening_labels_reingests_an_unchanged_file() {
        let (_d, path) = write_tmp_mbox(TWO_MSG_MBOX);
        let work = tempfile::tempdir().unwrap();
        let db_path = work.path().join("e.doltlite_db");

        // Only the `Sent` message is in scope. (Msg two carries
        // `Inbox,Sent`; msg one carries `Inbox,Starred,Unread`.)
        let first = run_once(&db_path, &path, |db| FetchOptions {
            only_labels: vec!["Sent".into()],
            ..FetchOptions::new(db)
        })
        .await;
        assert_eq!(first.emails_upserted, 1, "only the Sent message");

        // Widen to include Inbox; the file is byte-identical.
        let second = run_once(&db_path, &path, |db| FetchOptions {
            only_labels: vec!["Sent".into(), "Inbox".into()],
            ..FetchOptions::new(db)
        })
        .await;
        assert_eq!(
            second.emails_upserted, 2,
            "widened labels must re-read the file"
        );

        let db = RawDb::open(&db_path).await.unwrap();
        assert_eq!(db.load_emails().await.unwrap().len(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn narrowing_labels_drops_nothing() {
        let (_d, path) = write_tmp_mbox(TWO_MSG_MBOX);
        let work = tempfile::tempdir().unwrap();
        let db_path = work.path().join("e.doltlite_db");

        run_once(&db_path, &path, FetchOptions::new).await;
        let second = run_once(&db_path, &path, |db| FetchOptions {
            only_labels: vec!["Sent".into()],
            ..FetchOptions::new(db)
        })
        .await;
        assert_eq!(second.emails_upserted, 1, "only the Sent message");
        let db = RawDb::open(&db_path).await.unwrap();
        assert_eq!(
            db.load_emails().await.unwrap().len(),
            2,
            "narrowing must not drop already-ingested mail"
        );
    }

    /// An `mbox:` block edit reaches the account row though no message
    /// changed.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_account_edit_reaches_the_account_row() {
        let (_d, path) = write_tmp_mbox(TWO_MSG_MBOX);
        let work = tempfile::tempdir().unwrap();
        let db_path = work.path().join("e.doltlite_db");

        run_once(&db_path, &path, FetchOptions::new).await;

        run_once(&db_path, &path, |db| FetchOptions {
            account_config: MboxAccountConfig {
                display_name: Some("Work Gmail".into()),
                is_personal: Some(false),
                ..Default::default()
            },
            ..FetchOptions::new(db)
        })
        .await;

        let db = RawDb::open(&db_path).await.unwrap();
        let accounts = db.load_accounts().await.unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0]["name"], "Work Gmail");
    }

    fn msg_one() -> &'static str {
        &TWO_MSG_MBOX[..TWO_MSG_MBOX.find("From 2222@xxx").unwrap()]
    }

    fn msg_two() -> &'static str {
        &TWO_MSG_MBOX[TWO_MSG_MBOX.find("From 2222@xxx").unwrap()..]
    }

    const MSG_THREE: &str = concat!(
        "From 3333@xxx Thu Jun 04 08:00:00 +0000 2026\n",
        "X-GM-THRID: 3333\n",
        "X-Gmail-Labels: Archived\n",
        "Message-Id: <msg-three@enterprise.starfleet>\n",
        "From: Worf <worf@enterprise.starfleet>\n",
        "Subject: Security drill\n",
        "Date: Thu, 4 Jun 2026 08:00:00 +0000\n",
        "Content-Type: text/plain; charset=utf-8\n",
        "\n",
        "Drill at 0800.\n",
    );

    async fn stored(db_path: &Path) -> (Vec<String>, Vec<String>) {
        let db = RawDb::open(db_path).await.unwrap();
        let emails = sqlx::query_scalar("SELECT id FROM emails ORDER BY id")
            .fetch_all(db.pool())
            .await
            .unwrap();
        let threads = sqlx::query_scalar("SELECT id FROM threads ORDER BY id")
            .fetch_all(db.pool())
            .await
            .unwrap();
        db.close().await;
        (emails, threads)
    }

    /// #898: a gone `.mbox` file takes the emails only it held, and the
    /// last one takes everything.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_deleted_mbox_takes_the_emails_only_it_held() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.mbox"), msg_one()).unwrap();
        std::fs::write(dir.path().join("b.mbox"), msg_two()).unwrap();
        let work = tempfile::tempdir().unwrap();
        let db_path = work.path().join("e.doltlite_db");
        let run = || run_once(&db_path, dir.path(), FetchOptions::new);
        run().await;
        // 1111 = 0x457, 2222 = 0x8ae.
        assert_eq!(
            stored(&db_path).await.0,
            vec!["0000000000000457", "00000000000008ae"]
        );

        std::fs::remove_file(dir.path().join("b.mbox")).unwrap();
        let second = run().await;
        assert_eq!(second.emails_removed, 1);
        assert_eq!(
            stored(&db_path).await,
            (vec!["0000000000000457".into()], vec!["1111".into()])
        );

        std::fs::remove_file(dir.path().join("a.mbox")).unwrap();
        let third = run().await;
        assert_eq!(third.emails_removed, 1);
        assert_eq!(stored(&db_path).await, (vec![], vec![]));
    }

    /// A newer export written over the old one is the whole mailbox: a
    /// message it no longer carries was deleted.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_rewritten_mbox_drops_what_it_no_longer_holds() {
        let (dir, path) = write_tmp_mbox(TWO_MSG_MBOX);
        let work = tempfile::tempdir().unwrap();
        let db_path = work.path().join("e.doltlite_db");
        run_once(&db_path, &path, FetchOptions::new).await;

        std::fs::write(&path, msg_one()).unwrap();
        let second = run_once(&db_path, &path, FetchOptions::new).await;
        assert_eq!(second.emails_removed, 1);
        assert_eq!(
            stored(&db_path).await,
            (vec!["0000000000000457".into()], vec!["1111".into()])
        );
        drop(dir);
    }

    /// Two exports can hold one message; deleting one of them keeps it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_deleted_mbox_keeps_what_another_file_holds() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("all.mbox"), TWO_MSG_MBOX).unwrap();
        std::fs::write(dir.path().join("sent.mbox"), msg_two()).unwrap();
        let work = tempfile::tempdir().unwrap();
        let db_path = work.path().join("e.doltlite_db");
        let run = || run_once(&db_path, dir.path(), FetchOptions::new);
        run().await;

        std::fs::remove_file(dir.path().join("sent.mbox")).unwrap();
        let second = run().await;
        assert_eq!(second.emails_removed, 0);
        assert_eq!(stored(&db_path).await.0.len(), 2);
    }

    /// The re-read that decides what a gone file held counts every message,
    /// not only those the label filter lets in, so narrowing the filter in
    /// the same run deletes nothing that is still in a file.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_narrowed_filter_does_not_turn_a_deletion_into_a_purge() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.mbox"), TWO_MSG_MBOX).unwrap();
        std::fs::write(dir.path().join("b.mbox"), MSG_THREE).unwrap();
        let work = tempfile::tempdir().unwrap();
        let db_path = work.path().join("e.doltlite_db");
        run_once(&db_path, dir.path(), FetchOptions::new).await;
        assert_eq!(stored(&db_path).await.0.len(), 3);

        std::fs::remove_file(dir.path().join("b.mbox")).unwrap();
        let second = run_once(&db_path, dir.path(), |db| FetchOptions {
            only_labels: vec!["Sent".into()],
            ..FetchOptions::new(db)
        })
        .await;
        assert_eq!(second.emails_removed, 1, "only Worf's drill went");
        assert_eq!(
            stored(&db_path).await.0,
            vec!["0000000000000457", "00000000000008ae"]
        );
    }

    /// A `.mbox` rewritten to 0 bytes, or to text with no `From ` line,
    /// read as a clean file holding no messages, and the prune deleted what
    /// only it held. An mbox has no envelope that could say "no messages",
    /// so such a file is a problem and deletes nothing; deleting the file
    /// is what drops what it held.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_mbox_that_is_recognizably_nothing_deletes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.mbox"), msg_one()).unwrap();
        std::fs::write(dir.path().join("b.mbox"), msg_two()).unwrap();
        let work = tempfile::tempdir().unwrap();
        let db_path = work.path().join("e.doltlite_db");
        let run = || run_once(&db_path, dir.path(), FetchOptions::new);
        run().await;

        for (what, body) in [
            ("an empty file", ""),
            (
                "text with no From line",
                "Captain's log, stardate 41153.7\n",
            ),
        ] {
            std::fs::write(dir.path().join("b.mbox"), body).unwrap();
            let s = run().await;
            assert_eq!(s.emails_removed, 0, "{what} deleted emails");
            assert_eq!(stored(&db_path).await.0.len(), 2, "{what}");
            assert!(
                problems(&db_path)
                    .await
                    .iter()
                    .any(|(k, _)| k == "listing:mbox b.mbox"),
                "{what} is a problem on the file"
            );
        }
    }

    /// A changed `account_id` wrote the account row and read nothing, so
    /// every email stayed filed under the old account and nothing ever
    /// removed it. The files are read again under the new id and the old
    /// account goes.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_changed_account_id_moves_the_emails_and_retires_the_old_account() {
        let (_d, path) = write_tmp_mbox(TWO_MSG_MBOX);
        let work = tempfile::tempdir().unwrap();
        let db_path = work.path().join("e.doltlite_db");
        let account = |id: &str| MboxAccountConfig {
            account_id: Some(id.to_string()),
            ..Default::default()
        };
        run_once(&db_path, &path, |db| FetchOptions {
            account_config: account("enterprise"),
            ..FetchOptions::new(db)
        })
        .await;
        run_once(&db_path, &path, |db| FetchOptions {
            account_config: account("defiant"),
            ..FetchOptions::new(db)
        })
        .await;

        let db = RawDb::open(&db_path).await.unwrap();
        let accounts_of = |table: &'static str| {
            let pool = db.pool().clone();
            async move {
                sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(format!(
                    "SELECT DISTINCT account_id FROM {table} ORDER BY account_id"
                )))
                .fetch_all(&pool)
                .await
                .unwrap()
            }
        };
        assert_eq!(accounts_of("emails").await, ["defiant"]);
        assert_eq!(accounts_of("threads").await, ["defiant"]);
        assert_eq!(accounts_of("mailboxes").await, ["defiant"]);
        let accounts: Vec<String> = sqlx::query_scalar("SELECT id FROM accounts")
            .fetch_all(db.pool())
            .await
            .unwrap();
        assert_eq!(accounts, ["defiant"]);
        assert_eq!(db.load_emails().await.unwrap().len(), 2);
        db.close().await;
    }

    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_walk_error_deletes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.mbox"), msg_one()).unwrap();
        std::fs::write(dir.path().join("b.mbox"), msg_two()).unwrap();
        let work = tempfile::tempdir().unwrap();
        let db_path = work.path().join("e.doltlite_db");
        let run = || run_once(&db_path, dir.path(), FetchOptions::new);
        run().await;

        std::fs::remove_file(dir.path().join("b.mbox")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("nowhere"), dir.path().join("c.mbox")).unwrap();
        assert_eq!(run().await.emails_removed, 0);
        assert_eq!(stored(&db_path).await.0.len(), 2);
    }

    /// A read error can repeat on every read; the stream used to retry it
    /// forever, and the run with it.
    #[cfg(unix)]
    #[test]
    fn a_read_error_ends_the_stream() {
        let d = tempfile::tempdir().unwrap();
        // Opening a directory works; reading it does not.
        let items: Vec<Result<MboxMessage>> =
            iter_mbox_messages(d.path()).unwrap().take(3).collect();
        assert_eq!(items.len(), 1);
        assert!(items[0].is_err());
    }

    async fn problems(db_path: &Path) -> Vec<(String, String)> {
        let db = RawDb::open(db_path).await.unwrap();
        let rows = sqlx::query_as("SELECT scope_key, reason FROM problems ORDER BY scope_key")
            .fetch_all(db.pool())
            .await
            .unwrap();
        db.close().await;
        rows
    }

    /// `path` made unreadable, or `None` where the test runs as a user
    /// that reads it anyway (root, in a container).
    #[cfg(unix)]
    fn unreadable(path: &Path) -> Option<()> {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o000)).unwrap();
        if File::open(path).is_ok() {
            tracing::info!("skipped: this user reads a mode-000 file");
            return None;
        }
        Some(())
    }

    #[cfg(unix)]
    fn readable(path: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }

    /// A folder whose only entry could not be walked has no file to read,
    /// and the run that returned early for that said nothing of the entry.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_walk_error_with_no_file_to_read_is_still_a_row() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("c.mbox");
        std::os::unix::fs::symlink(dir.path().join("nowhere"), &link).unwrap();
        let work = tempfile::tempdir().unwrap();
        let db_path = work.path().join("e.doltlite_db");
        let run = || run_once(&db_path, dir.path(), FetchOptions::new);
        run().await;
        assert_eq!(
            problems(&db_path).await,
            [("listing:files".to_string(), "fetch_failed".to_string())]
        );

        std::fs::remove_file(&link).unwrap();
        run().await;
        assert!(problems(&db_path).await.is_empty());
    }

    /// A file that will not open costs that file: the run goes on, says
    /// so, and reads it again next time.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_file_that_will_not_open_is_a_row_not_a_failed_run() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.mbox"), TWO_MSG_MBOX).unwrap();
        std::fs::write(dir.path().join("b.mbox"), MSG_THREE).unwrap();
        let work = tempfile::tempdir().unwrap();
        let db_path = work.path().join("e.doltlite_db");
        let labels = |l: &[&str]| l.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        run_once(&db_path, dir.path(), |db| FetchOptions {
            only_labels: labels(&["Sent"]),
            ..FetchOptions::new(db)
        })
        .await;

        // The filter widens to take in Worf's drill, which only b.mbox
        // holds.
        let b = dir.path().join("b.mbox");
        if unreadable(&b).is_none() {
            return;
        }
        let widened = |db| FetchOptions {
            only_labels: labels(&["Sent", "Archived"]),
            ..FetchOptions::new(db)
        };
        run_once(&db_path, dir.path(), widened).await;
        assert_eq!(
            problems(&db_path).await,
            [(
                "listing:mbox b.mbox".to_string(),
                "fetch_failed".to_string()
            )]
        );
        assert_eq!(
            stored(&db_path).await.0.len(),
            1,
            "Worf's drill is in b.mbox"
        );

        // The widening is owed until a run reads b.mbox.
        readable(&b);
        run_once(&db_path, dir.path(), widened).await;
        assert!(problems(&db_path).await.is_empty());
        assert_eq!(stored(&db_path).await.0.len(), 2);
    }

    /// A message that will not parse is a row on its file, which stands
    /// until a run reads the file without it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_message_that_will_not_parse_is_a_row_on_its_file() {
        // The first `From ` line opens a message with nothing in it.
        let body = format!("From 9999@xxx Wed Jun 03 22:00:00 +0000 2026\n{TWO_MSG_MBOX}");
        let (_d, path) = write_tmp_mbox(&body);
        let work = tempfile::tempdir().unwrap();
        let db_path = work.path().join("e.doltlite_db");
        let first = run_once(&db_path, &path, FetchOptions::new).await;
        assert_eq!(first.parse_errors, 1);
        assert_eq!(first.emails_upserted, 2);
        let rows = problems(&db_path).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(rows[0].0.starts_with("skipped:mbox:"), "{rows:?}");
        assert_eq!(rows[0].1, "undeserializable");

        std::fs::write(&path, TWO_MSG_MBOX).unwrap();
        run_once(&db_path, &path, FetchOptions::new).await;
        assert!(problems(&db_path).await.is_empty());
    }

    /// A rewritten file read on a run that could not read another deletes
    /// nothing; the first run that reads every file drops what it no
    /// longer holds.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn a_held_back_deletion_happens_once_every_file_reads() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.mbox");
        let b = dir.path().join("b.mbox");
        std::fs::write(&a, TWO_MSG_MBOX).unwrap();
        std::fs::write(&b, MSG_THREE).unwrap();
        let work = tempfile::tempdir().unwrap();
        let db_path = work.path().join("e.doltlite_db");
        let run = || run_once(&db_path, dir.path(), FetchOptions::new);
        run().await;
        assert_eq!(stored(&db_path).await.0.len(), 3);

        std::fs::write(&a, msg_one()).unwrap();
        if unreadable(&b).is_none() {
            return;
        }
        assert_eq!(run().await.emails_removed, 0);
        assert_eq!(
            problems(&db_path)
                .await
                .into_iter()
                .map(|p| p.0)
                .collect::<Vec<_>>(),
            ["listing:mbox b.mbox", "listing:removed_records"]
        );

        readable(&b);
        assert_eq!(run().await.emails_removed, 1, "Riker's reply left a.mbox");
        assert!(problems(&db_path).await.is_empty());
    }

    /// A configured label no message carries is a row, as the other two
    /// modes make one, and correcting the config clears it.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_label_no_message_carries_is_a_row() {
        let (_d, path) = write_tmp_mbox(TWO_MSG_MBOX);
        let work = tempfile::tempdir().unwrap();
        let db_path = work.path().join("e.doltlite_db");
        run_once(&db_path, &path, |db| FetchOptions {
            only_labels: vec!["Sent".into(), "Starbase".into()],
            ..FetchOptions::new(db)
        })
        .await;
        assert_eq!(
            problems(&db_path).await,
            [(
                "config:only_extract_labels:Starbase".to_string(),
                "not_found".to_string()
            )]
        );

        run_once(&db_path, &path, FetchOptions::new).await;
        assert!(problems(&db_path).await.is_empty());
    }

    /// Every run reads every file, so a file that stopped reading is a row
    /// even though its bytes never changed, and the emails only it holds
    /// stay until it reads again.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread")]
    async fn an_unchanged_file_that_will_not_read_is_a_row() {
        let dir = tempfile::tempdir().unwrap();
        let b = dir.path().join("b.mbox");
        std::fs::write(dir.path().join("a.mbox"), TWO_MSG_MBOX).unwrap();
        std::fs::write(&b, MSG_THREE).unwrap();
        let work = tempfile::tempdir().unwrap();
        let db_path = work.path().join("e.doltlite_db");
        let run = || run_once(&db_path, dir.path(), FetchOptions::new);
        run().await;

        if unreadable(&b).is_none() {
            return;
        }
        assert_eq!(run().await.emails_removed, 0);
        assert_eq!(
            problems(&db_path)
                .await
                .into_iter()
                .map(|p| p.0)
                .collect::<Vec<_>>(),
            ["listing:mbox b.mbox", "listing:removed_records"]
        );
        assert_eq!(stored(&db_path).await.0.len(), 3);

        readable(&b);
        assert_eq!(run().await.emails_removed, 0);
        assert!(problems(&db_path).await.is_empty());
    }

    /// The account to retire was read from a record of the last id, which
    /// a run that could not read every file left unchanged: an id changed
    /// twice across such a run left the middle account's row and labels
    /// behind for good. What the store files under any other account goes.
    #[tokio::test(flavor = "multi_thread")]
    async fn every_account_the_config_no_longer_names_is_retired() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.mbox"), TWO_MSG_MBOX).unwrap();
        let work = tempfile::tempdir().unwrap();
        let db_path = work.path().join("e.doltlite_db");
        let under = |id: &str| {
            let (db_path, path) = (db_path.clone(), dir.path().to_path_buf());
            let account_config = MboxAccountConfig {
                account_id: Some(id.to_string()),
                ..Default::default()
            };
            async move {
                run_once(&db_path, &path, |db| FetchOptions {
                    account_config,
                    ..FetchOptions::new(db)
                })
                .await
            }
        };
        under("enterprise").await;
        // A file holding nothing holds the prune, and the retirement with it.
        std::fs::write(dir.path().join("c.mbox"), "").unwrap();
        under("defiant").await;
        std::fs::remove_file(dir.path().join("c.mbox")).unwrap();
        under("voyager").await;

        let db = RawDb::open(&db_path).await.unwrap();
        for table in ["emails", "threads", "mailboxes"] {
            let accounts: Vec<String> = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                "SELECT DISTINCT account_id FROM {table} ORDER BY account_id"
            )))
            .fetch_all(db.pool())
            .await
            .unwrap();
            assert_eq!(accounts, ["voyager"], "{table}");
        }
        let accounts: Vec<String> = sqlx::query_scalar("SELECT id FROM accounts ORDER BY id")
            .fetch_all(db.pool())
            .await
            .unwrap();
        assert_eq!(accounts, ["voyager"]);
        db.close().await;
    }
}
