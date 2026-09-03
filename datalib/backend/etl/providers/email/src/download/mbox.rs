//! mbox extractor. Walks a Google Takeout `.mbox` file (RFC 4155
//! mboxrd framing, with `X-GM-THRID` / `X-Gmail-Labels` Gmail
//! extensions) and lands every message into the shared email raw
//! store as if it had come off a JMAP server — typed envelope
//! columns + join rows + the RFC 5322 `.eml` bytes in the blob CAS.
//! No body parsing, no html2md, no JMAP-shape payload synthesis;
//! render handles all of that downstream off the `.eml` blob.
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

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{anyhow, Context, Result};
use datalib_etl::blob_cas::{blake3_hex, CasEdgeAccumulator, CasEdgeRow as _};
use datalib_etl::bulk::{
    bulk_upsert_entity_in_tx, push_placeholder_list, push_placeholders, SQL_CHUNK,
};
use datalib_etl::control::DownloadControl;
use datalib_etl::progress::Progress;
use mail_parser::MessageParser;
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::{Sqlite, Transaction};
use tracing::{info, warn};

use super::db::{db_path_for, EmailRow, RawDb};
use super::envelope::{self, header_text, strip_angle};
use super::labels::{mailbox_id, map_label, split_gmail_labels, LabelMap};
use super::schema_raw::{
    AccountRow, EmailKeywordRow, EmailMailboxRow, EmlBlobRow, MboxFilesCheckpointRow,
};

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

/// Account-row data the orchestrator pipes in from the source YAML.
///
/// Mbox files don't carry an account identity inside them — the file
/// stem is the only thing we can derive from the file alone, and even
/// that is brittle. The sync YAML names the account (display name,
/// canonical email address, personal-vs-shared flag), and this struct
/// carries that information into the mbox download so the synthesized
/// `accounts` row matches the shape JMAP would produce.
///
/// All fields are optional so the download still runs against a
/// loose `.mbox` with no configured account (e.g. a one-off
/// fixture). Defaults: `account_id` ← mbox file stem (or
/// `account_id_override`); `display_name` ← `account_id`; `is_personal`
/// ← `true`.
#[derive(Debug, Clone, Default)]
pub struct MboxAccountConfig {
    pub account_id: Option<String>,
    pub display_name: Option<String>,
    pub email_address: Option<String>,
    pub is_personal: Option<bool>,
}

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Doltlite database path. Ignored when `db` is `Some`.
    pub db_path: PathBuf,
    /// Pre-opened raw DB (sync orchestrator populates this so the
    /// post-download commit hits the same pool).
    pub db: Option<RawDb>,
    /// `.mbox` file (or directory containing `*.mbox` files).
    pub input_path: PathBuf,
    /// Overrides the file-stem default for `account_id`. (Kept
    /// alongside `account_config` for back-compat with tests / older
    /// call sites; if `account_config.account_id` is set, that wins.)
    pub account_id_override: Option<String>,
    /// Account-row config from the source YAML (display name, email,
    /// is_personal flag). See [`MboxAccountConfig`].
    pub account_config: MboxAccountConfig,
    /// When non-empty, only ingest messages carrying at least one
    /// `X-Gmail-Labels` label whose full path (POSIX-like, e.g.
    /// `Work/Projects`) exactly matches one of these. Empty = ingest
    /// every message. Mirrors the JMAP `only_mailbox_labels` filter;
    /// Gmail nested labels are already stored as `Parent/Child` strings
    /// so the match is a direct string compare against the raw label.
    pub only_labels: Vec<String>,
    /// Skip attachment bytes whose size exceeds this. The
    /// `email_attachments` row still lands (so we record what was
    /// referenced), but the bytes never enter the CAS — render
    /// will render `_(blob not materialized)_` for them.
    pub blob_size_limit_bytes: Option<u64>,
    pub progress: Progress,
    pub control: DownloadControl,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            db_path: PathBuf::new(),
            db: None,
            input_path: PathBuf::new(),
            account_id_override: None,
            account_config: MboxAccountConfig::default(),
            only_labels: Vec::new(),
            blob_size_limit_bytes: None,
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
    pub blobs_oversize: usize,
    pub parse_errors: usize,
}

/// Scope key for the mbox path's [`datalib_etl::scope_config`]
/// record. Distinct from the JMAP path's `jmap:download` — the two
/// modes of `type: email` keep separate state.
const SCOPE_CONFIG_KEY: &str = "mbox:download";

/// Blob keys. Named so writer and reader can't drift.
const K_ONLY_LABELS: &str = "only_extract_labels";
const K_BLOB_CAP: &str = "blob_size_limit_bytes";
const K_ACCOUNT: &str = "account";

/// The subset of [`FetchOptions`] that decides which data lands on disk.
///
/// The per-file `(size, mtime)` checkpoint answers "did the input
/// change?", which is all it can answer. It cannot answer "is the output
/// already correct?" — and those diverge the moment config participates
/// in the transformation. This record covers the difference.
fn scope_config_blob(opts: &FetchOptions) -> Value {
    // Sorted so a reordered config list isn't mistaken for a change.
    let mut labels: Vec<&str> = opts.only_labels.iter().map(String::as_str).collect();
    labels.sort_unstable();
    json!({
        K_ONLY_LABELS: labels,
        K_BLOB_CAP: opts.blob_size_limit_bytes,
        // The account row is derived wholly from these, so comparing the
        // rendered values is exactly right.
        K_ACCOUNT: {
            "account_id": opts.account_config.account_id,
            "display_name": opts.account_config.display_name,
            "email_address": opts.account_config.email_address,
            "is_personal": opts.account_config.is_personal,
        },
    })
}

/// What a config change since the last satisfying run requires.
///
/// Only *widenings* need the files re-read; a narrowed filter or a
/// tightened cap leaves an on-disk superset, and nothing in the
/// pipeline deletes. The account fields are separable: they feed
/// `flush_account_and_lookups`, which is independent of message ingest,
/// so editing an `mbox:` block costs one UPSERT rather than a re-read of
/// a multi-gigabyte export.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Adjustments {
    /// Ignore the per-file checkpoints and re-read every mbox.
    reingest_files: bool,
    /// Re-run the account/lookup flush even if every file was skipped.
    refresh_account: bool,
}

impl Adjustments {
    fn plan(prior: Option<&Value>, opts: &FetchOptions) -> Self {
        let mut out = Self::default();
        let Some(prior) = prior else {
            // Every store predating this record. Adopt, do nothing.
            return out;
        };

        use datalib_etl::scope_config::FilterChange;
        match datalib_etl::scope_config::filter_widened(
            Some(prior),
            K_ONLY_LABELS,
            &opts.only_labels,
        ) {
            FilterChange::Unchanged => {}
            FilterChange::WidenedToAll => {
                out.reingest_files = true;
                info!(
                    event = "mbox_labels_widened",
                    added = "<filter removed>",
                    "re-reading mbox files; every label is now in scope",
                );
            }
            FilterChange::Added(added) => {
                out.reingest_files = true;
                info!(
                    event = "mbox_labels_widened",
                    added = ?added,
                    "re-reading mbox files for newly-in-scope labels",
                );
            }
        }

        if datalib_etl::scope_config::limit_relaxed(
            Some(prior),
            K_BLOB_CAP,
            opts.blob_size_limit_bytes,
        ) {
            out.reingest_files = true;
            info!(
                event = "mbox_blob_limit_relaxed",
                limit = ?opts.blob_size_limit_bytes,
                "re-reading mbox files for previously-oversize attachments",
            );
        }

        let cur_account = scope_config_blob(opts);
        if prior.get(K_ACCOUNT) != cur_account.get(K_ACCOUNT) {
            // Deliberately does NOT set `reingest_files`: the account row
            // is written by `flush_account_and_lookups`, which doesn't
            // read a single message.
            out.refresh_account = true;
            info!(
                event = "mbox_account_config_changed",
                "refreshing the account row without re-reading any mbox",
            );
        }

        out
    }
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
        .account_config
        .account_id
        .clone()
        .or_else(|| opts.account_id_override.clone())
        .unwrap_or_else(|| default_account_id(&opts.input_path));

    // Diff the scope-affecting params against the ones that produced the
    // current checkpoints.
    let scope_cfg = scope_config_blob(&opts);
    let prior_scope_cfg =
        datalib_etl::scope_config::load_or_none(db.pool(), SCOPE_CONFIG_KEY).await;
    let adjust = Adjustments::plan(prior_scope_cfg.as_ref(), &opts);

    let known_blobs = db.loaded_blob_ids().await?;

    // Per-file (size, mtime_ns) fingerprints. Files whose stamped
    // checkpoint still matches the current fingerprint are skipped
    // outright — mail clients only append to mbox, so `(size, mtime)`
    // is a sufficient unchanged-ness signal without re-hashing
    // contents. Files that can't be stat'd or canonicalized fall
    // through to the process bucket and fail loudly downstream.
    let stamped = load_mbox_checkpoints(&db).await?;
    let mut to_process: Vec<MboxJob> = Vec::with_capacity(mbox_paths.len());
    let mut skipped_total_bytes: u64 = 0;
    let mut skipped_count: usize = 0;
    for path in &mbox_paths {
        let job = match prepare_mbox_job(path) {
            Ok(j) => j,
            Err(e) => {
                warn!(event = "mbox_stat_failed", path = %path.display(), error = %e);
                continue;
            }
        };
        if !adjust.reingest_files
            && stamped
                .get(&job.canonical)
                .is_some_and(|(sz, mt)| *sz == job.size_bytes && *mt == job.mtime_ns)
        {
            info!(
                event = "mbox_file_skipped",
                path = %job.path.display(),
                size_bytes = job.size_bytes,
                "fingerprint matches checkpoint; skipping",
            );
            skipped_total_bytes = skipped_total_bytes.saturating_add(job.size_bytes);
            skipped_count += 1;
            continue;
        }
        to_process.push(job);
    }

    // Progress bar runs over bytes-consumed-from-mbox-files (a known
    // total from the filesystem, so it has a real endpoint and ETA)
    // rather than emails-processed (which we don't know up front and
    // would only resolve at EOF). Per-batch `set_message` reports the
    // running email count as supplemental progress info. Skipped
    // files' bytes are baked into `set_length` and pre-incremented
    // up front so the bar reflects "100% means done with this run."
    let total_bytes: u64 = to_process
        .iter()
        .map(|j| j.size_bytes)
        .sum::<u64>()
        .saturating_add(skipped_total_bytes);
    opts.progress.set_length(Some(total_bytes));
    if skipped_total_bytes > 0 {
        opts.progress.inc(skipped_total_bytes);
    }

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
    let mut accumulator = Accumulator::new(account_id.clone(), label_filter);
    let mut summary = FetchSummary::default();
    let mut batch = PendingBatch::default();
    let mut emails_seen: u64 = 0;
    let mut files_processed: usize = 0;

    for job in &to_process {
        for raw in iter_mbox_messages(&job.path)? {
            let (raw, bytes_consumed) = match raw {
                Ok((bytes, consumed)) => (bytes, consumed),
                Err(e) => {
                    warn!(event = "mbox_read_failed", path = %job.path.display(), error = %e);
                    summary.parse_errors += 1;
                    continue;
                }
            };
            opts.progress.inc(bytes_consumed);
            match accumulator.ingest_message(&raw, &known_blobs, &mut batch, &mut summary) {
                Ok(true) => {
                    emails_seen += 1;
                    opts.progress.set_message(&format!("{emails_seen} emails"));
                    if batch.emails.len() >= FLUSH_BATCH {
                        flush_batch(&db, &mut batch, &mut summary).await?;
                    }
                }
                Ok(false) => {} // duplicate; skipped
                Err(e) => {
                    warn!(event = "mbox_message_failed", error = %e);
                    summary.parse_errors += 1;
                }
            }
        }
        // Flush at the file boundary so the checkpoint we stamp next
        // is causally after every row this file produced. Without
        // this, a Ctrl-C between two files' messages could leave the
        // checkpoint ahead of the data.
        flush_batch(&db, &mut batch, &mut summary).await?;
        upsert_mbox_checkpoint(&db, job).await?;
        files_processed += 1;
    }
    flush_batch(&db, &mut batch, &mut summary).await?;

    // Account + mailboxes + threads + matching bookkeeping all land
    // in one closing transaction. Skip it entirely when nothing was
    // processed — the accumulator is empty, and even the no-op
    // upserts (which are idempotent ON CONFLICT chains, not
    // delete-then-insert) aren't worth the round-trip when every
    // file was a cache hit.
    if files_processed > 0 || adjust.refresh_account {
        // With no files processed the accumulator is empty, so this
        // writes the account row and nothing else — which is exactly
        // what an `mbox:` block edit needs. Every write here is an
        // idempotent ON CONFLICT chain, never delete-then-insert, so
        // running it over an empty accumulator can't drop anything.
        flush_account_and_lookups(
            &db,
            &account_id,
            &opts.account_config,
            &accumulator,
            &mut summary,
        )
        .await?;
    }
    if files_processed == 0 && skipped_count > 0 {
        info!(
            event = "mbox_all_files_skipped",
            skipped_count,
            account_refreshed = adjust.refresh_account,
            "every mbox file matched its checkpoint",
        );
    }

    // Record the config only once this run satisfied it, so a failure
    // leaves the previous record in place and the next run re-plans.
    // (Errors above return early, so reaching here means success.)
    datalib_etl::scope_config::store_if_satisfied(db.pool(), SCOPE_CONFIG_KEY, &scope_cfg, true)
        .await;

    Ok(summary)
}

/// One mbox file scheduled for ingest, paired with the fingerprint
/// that will be stamped into `mbox_files_checkpoint` after it
/// drains successfully.
struct MboxJob {
    path: PathBuf,
    /// Canonical absolute path — the checkpoint table's primary key.
    /// Canonicalization happens once at scheduling time so relative
    /// vs absolute spellings of the same file hit the same row
    /// across runs.
    canonical: String,
    size_bytes: u64,
    mtime_ns: i64,
}

fn prepare_mbox_job(path: &Path) -> Result<MboxJob> {
    let meta = std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?;
    let mtime = meta
        .modified()
        .with_context(|| format!("mtime {}", path.display()))?;
    let mtime_ns = match mtime.duration_since(UNIX_EPOCH) {
        Ok(d) => i64::try_from(d.as_nanos()).unwrap_or(i64::MAX),
        // Pre-1970 mtime is exotic enough that we treat it as
        // "never matches" rather than panic — the file will be
        // ingested every run, which is the safe default.
        Err(_) => i64::MIN,
    };
    let canonical = std::fs::canonicalize(path)
        .with_context(|| format!("canonicalize {}", path.display()))?
        .to_string_lossy()
        .into_owned();
    Ok(MboxJob {
        path: path.to_path_buf(),
        canonical,
        size_bytes: meta.len(),
        mtime_ns,
    })
}

async fn load_mbox_checkpoints(db: &RawDb) -> Result<HashMap<String, (u64, i64)>> {
    let rows = sqlx::query_as::<_, (String, i64, i64)>(
        "SELECT path, size_bytes, mtime_ns FROM mbox_files_checkpoint",
    )
    .fetch_all(db.pool())
    .await
    .context("load mbox_files_checkpoint")?;
    Ok(rows
        .into_iter()
        .map(|(p, sz, mt)| (p, (sz as u64, mt)))
        .collect())
}

async fn upsert_mbox_checkpoint(db: &RawDb, job: &MboxJob) -> Result<()> {
    let now = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    let row =
        MboxFilesCheckpointRow::new(&job.canonical, job.size_bytes as i64, job.mtime_ns, &now);
    let mut tx = db
        .pool()
        .begin()
        .await
        .context("begin mbox checkpoint tx")?;
    bulk_upsert_entity_in_tx(&mut tx, std::slice::from_ref(&row))
        .await
        .with_context(|| format!("upsert mbox checkpoint {}", job.path.display()))?;
    tx.commit()
        .await
        .with_context(|| format!("commit mbox checkpoint {}", job.path.display()))?;
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────
// Streaming mbox iterator
// ─────────────────────────────────────────────────────────────────────

/// Iterate `path` yielding one RFC 5322 message at a time. Each yield
/// also reports the number of mbox-stream bytes consumed since the
/// previous yield, so the caller can advance a byte-keyed progress
/// bar against the known file size. The mbox envelope `From ` line is
/// stripped; `>From `-style escapes are unquoted. Streams off disk via
/// `BufReader` so peak RSS stays bounded regardless of file size.
fn iter_mbox_messages(path: &Path) -> Result<impl Iterator<Item = Result<(Vec<u8>, u64)>>> {
    let file = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mut reader = BufReader::with_capacity(1 << 16, file);
    let mut pending: Option<Vec<u8>> = None;
    let mut buf: Vec<u8> = Vec::with_capacity(4096);
    let mut started = false;
    // `bytes_since_yield` accumulates every byte read from the file
    // (including the envelope `From ` lines, blank separators, and any
    // pre-first-message junk) and resets at each yield. The caller
    // sums these into a "bytes processed" progress increment, which
    // ends up matching the file's `metadata().len()` once iteration
    // finishes — regardless of how many emails ended up in the file.
    let mut bytes_since_yield: u64 = 0;
    let it = std::iter::from_fn(move || loop {
        buf.clear();
        let n = match reader.read_until(b'\n', &mut buf) {
            Ok(0) => {
                // EOF; flush any pending message together with the
                // remaining bytes counted on this last `read_until`
                // (which returned 0 — nothing to add).
                let take_bytes = std::mem::take(&mut bytes_since_yield);
                return pending.take().map(|msg| Ok((msg, take_bytes)));
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
            pending = Some(Vec::with_capacity(4096));
            started = true;
            if let Some(msg) = prev {
                let take_bytes = std::mem::take(&mut bytes_since_yield);
                return Some(Ok((msg, take_bytes)));
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
    mailboxes: BTreeMap<String, MailboxEntry>,
    threads: BTreeMap<String, Vec<ThreadMember>>,
    seen_email_ids: BTreeSet<String>,
    /// When `Some`, only messages carrying a label whose full path is
    /// in this set are ingested (the rest are dropped before any row or
    /// blob lands). `None` = ingest everything. See
    /// [`FetchOptions::only_labels`].
    label_filter: Option<HashSet<String>>,
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
    fn new(account_id: String, label_filter: Option<HashSet<String>>) -> Self {
        Self {
            account_id,
            mailboxes: BTreeMap::new(),
            threads: BTreeMap::new(),
            seen_email_ids: BTreeSet::new(),
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
                None,
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
    /// `.eml` bytes + their `email_blobs` edges for this batch. The
    /// accumulator holds the bytes (in its [`BlobBundle`]) and resolves
    /// each edge's `blake3` off them at flush time — see
    /// [`datalib_etl::blob_cas::CasEdgeAccumulator`].
    cas: CasEdgeAccumulator,
    /// In-run dedupe of blob ref ids. JMAP `Email.blobId` is server-
    /// opaque (different per email), but for mbox sources the ref_id
    /// is `sha256(bytes)` — identical bodies / attachments collapse
    /// to a single row, and this set keeps the edge list itself
    /// dedup-free so doltlite never sees a conflicting bind pair
    /// inside one multi-row statement.
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
///
/// No JSONL wire-tape: mbox is a file on disk, not a wire — there
/// are no upstream events to mirror.
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

/// Flush the account row, the per-label mailbox rows, and the per-
/// thread rows once the message walk is done. Three small tables;
/// one transaction.
async fn flush_account_and_lookups(
    db: &RawDb,
    account_id: &str,
    account_config: &MboxAccountConfig,
    accumulator: &Accumulator,
    summary: &mut FetchSummary,
) -> Result<()> {
    let now = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    let mut tx = db.pool().begin().await.context("begin lookups tx")?;

    // Account row: route through `AccountRow::from_mbox_config` and
    // the shared `bulk_upsert_in_tx` so the synthesized row has the
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
    datalib_etl::bulk::bulk_upsert_in_tx(&mut tx, &[account_row], &now).await?;

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
    datalib_etl::bulk::bulk_upsert_bookkeeping(
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
    datalib_etl::bulk::bulk_upsert_bookkeeping(
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
    // Standard `bulk_upsert_in_tx` path — `EmailRow` carries its
    // own `BulkUpsertable` impl. The framework picks the right
    // column list + binding sequence; the conflict clause uses the
    // universal "every non-PK col = excluded.<col>" shape from
    // `data_architecture_ingestion.md` §"One writer per row".
    let now = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    datalib_etl::bulk::bulk_upsert_in_tx(tx, rows, &now).await
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
        push_placeholders(&mut sql, chunk.len(), 4);
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

// ─────────────────────────────────────────────────────────────────────
// Label mapping
// ─────────────────────────────────────────────────────────────────────

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
/// containing one. Sync's download dispatch uses this to pick between
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
            .unwrap()
            .into_iter()
            .map(|(bytes, _consumed)| bytes)
            .collect();
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
            .unwrap()
            .into_iter()
            .map(|(bytes, _consumed)| bytes)
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
        for _ in 0..2 {
            let db = RawDb::open(&db_path).await.unwrap();
            let pool = db.pool().clone();
            let s = fetch(FetchOptions {
                db_path: db_path.clone(),
                db: Some(db),
                input_path: path.clone(),
                ..Default::default()
            })
            .await
            .unwrap();
            summaries.push(s);
            pool.close().await;
        }
        let db = RawDb::open(&db_path).await.unwrap();
        assert_eq!(db.load_emails().await.unwrap().len(), 2);

        // First run did real work; second run hit the checkpoint and
        // skipped every file. The mbox file's (size, mtime) is
        // unchanged between the two runs, so the cursor short-
        // circuits before `iter_mbox_messages` opens it.
        assert_eq!(summaries[0].emails_upserted, 2);
        assert_eq!(summaries[1].emails_upserted, 0);
        assert_eq!(summaries[1].blobs_stored, 0);
        assert_eq!(summaries[1].mailboxes_upserted, 0);
        assert_eq!(summaries[1].threads_upserted, 0);

        // And the cursor row is present after the first run.
        let stamped: i64 = sqlx::query_scalar("SELECT count(*) FROM mbox_files_checkpoint")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert_eq!(stamped, 1);
    }

    /// Run `fetch` once against `path`, opening and closing its own
    /// pool so the next run sees a clean connection. Mirrors
    /// `re_running_is_idempotent`.
    async fn run_once(db_path: &Path, path: &Path, opts: FetchOptions) -> FetchSummary {
        let db = RawDb::open(db_path).await.unwrap();
        let pool = db.pool().clone();
        let s = fetch(FetchOptions {
            db_path: db_path.to_path_buf(),
            db: Some(db),
            input_path: path.to_path_buf(),
            ..opts
        })
        .await
        .unwrap();
        pool.close().await;
        s
    }

    /// The headline case: an unchanged file whose config widened must be
    /// re-read. Guards the plumbing, not just `Adjustments::plan` — the
    /// gate fails silently to a no-op, so a dropped `!adjust.reingest_files`
    /// would leave every unit test green while restoring the bug.
    #[tokio::test(flavor = "multi_thread")]
    async fn widening_labels_reingests_an_unchanged_file() {
        let (_d, path) = write_tmp_mbox(TWO_MSG_MBOX);
        let work = tempfile::tempdir().unwrap();
        let db_path = work.path().join("e.doltlite_db");

        // Only the `Sent` message is in scope. (Msg two carries
        // `Inbox,Sent`; msg one carries `Inbox,Starred,Unread`.)
        let first = run_once(
            &db_path,
            &path,
            FetchOptions {
                only_labels: vec!["Sent".into()],
                ..Default::default()
            },
        )
        .await;
        assert_eq!(first.emails_upserted, 1, "only the Sent message");

        // Widen to include Inbox. The file is byte-identical, so the
        // (size, mtime) checkpoint alone would skip it forever.
        let second = run_once(
            &db_path,
            &path,
            FetchOptions {
                only_labels: vec!["Sent".into(), "Inbox".into()],
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            second.emails_upserted, 2,
            "widened labels must re-read the file"
        );

        let db = RawDb::open(&db_path).await.unwrap();
        assert_eq!(db.load_emails().await.unwrap().len(), 2);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn narrowing_labels_does_not_reingest() {
        let (_d, path) = write_tmp_mbox(TWO_MSG_MBOX);
        let work = tempfile::tempdir().unwrap();
        let db_path = work.path().join("e.doltlite_db");

        run_once(&db_path, &path, FetchOptions::default()).await;
        let second = run_once(
            &db_path,
            &path,
            FetchOptions {
                only_labels: vec!["Sent".into()],
                ..Default::default()
            },
        )
        .await;
        // The store is already a superset; re-reading would produce
        // nothing. This is the case a config *hash* would get wrong.
        assert_eq!(second.emails_upserted, 0);
        let db = RawDb::open(&db_path).await.unwrap();
        assert_eq!(
            db.load_emails().await.unwrap().len(),
            2,
            "narrowing must not drop already-ingested mail"
        );
    }

    /// The case that motivated storing values instead of a hash: an
    /// `mbox:` block edit updates one row and never opens the file.
    #[tokio::test(flavor = "multi_thread")]
    async fn account_edit_refreshes_without_reingesting() {
        let (_d, path) = write_tmp_mbox(TWO_MSG_MBOX);
        let work = tempfile::tempdir().unwrap();
        let db_path = work.path().join("e.doltlite_db");

        run_once(&db_path, &path, FetchOptions::default()).await;

        let second = run_once(
            &db_path,
            &path,
            FetchOptions {
                account_config: MboxAccountConfig {
                    display_name: Some("Work Gmail".into()),
                    is_personal: Some(false),
                    ..Default::default()
                },
                ..Default::default()
            },
        )
        .await;
        assert_eq!(
            second.emails_upserted, 0,
            "an account-field edit must not re-read the mbox"
        );

        let db = RawDb::open(&db_path).await.unwrap();
        let accounts = db.load_accounts().await.unwrap();
        assert_eq!(accounts.len(), 1);
        assert_eq!(accounts[0]["name"], "Work Gmail");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn unchanged_config_still_skips() {
        // Guards the other direction: the adjustment must not fire on
        // every run and turn each sync into a full re-read.
        let (_d, path) = write_tmp_mbox(TWO_MSG_MBOX);
        let work = tempfile::tempdir().unwrap();
        let db_path = work.path().join("e.doltlite_db");
        let opts = || FetchOptions {
            only_labels: vec!["Inbox".into()],
            ..Default::default()
        };
        assert_eq!(run_once(&db_path, &path, opts()).await.emails_upserted, 2);
        assert_eq!(run_once(&db_path, &path, opts()).await.emails_upserted, 0);
        assert_eq!(run_once(&db_path, &path, opts()).await.emails_upserted, 0);
    }
}

#[cfg(test)]
mod scope_config_tests {
    use super::*;
    use serde_json::json;

    fn opts(labels: &[&str], cap: Option<u64>, account: MboxAccountConfig) -> FetchOptions {
        FetchOptions {
            only_labels: labels.iter().map(|s| s.to_string()).collect(),
            blob_size_limit_bytes: cap,
            account_config: account,
            ..Default::default()
        }
    }

    fn named(display: Option<&str>) -> MboxAccountConfig {
        MboxAccountConfig {
            display_name: display.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn absent_record_plans_nothing() {
        // Every mbox store predating this record. Must not re-read a
        // multi-gigabyte export on upgrade.
        let o = opts(&["Sent"], Some(1000), named(None));
        assert_eq!(Adjustments::plan(None, &o), Adjustments::default());
    }

    #[test]
    fn unchanged_config_plans_nothing() {
        let o = opts(&["Sent"], Some(1000), named(Some("Work")));
        let prior = scope_config_blob(&o);
        assert_eq!(Adjustments::plan(Some(&prior), &o), Adjustments::default());
    }

    #[test]
    fn label_order_is_not_a_change() {
        let prior = scope_config_blob(&opts(&["Sent", "Inbox"], None, named(None)));
        let o = opts(&["Inbox", "Sent"], None, named(None));
        assert_eq!(Adjustments::plan(Some(&prior), &o), Adjustments::default());
    }

    // ── the headline case ────────────────────────────────────────────

    #[test]
    fn widened_labels_reingest_files() {
        let prior = scope_config_blob(&opts(&["Sent"], None, named(None)));
        let plan = Adjustments::plan(Some(&prior), &opts(&["Sent", "Inbox"], None, named(None)));
        assert!(plan.reingest_files);
        assert!(!plan.refresh_account);
    }

    #[test]
    fn adding_to_an_empty_filter_is_a_narrowing() {
        // `[]` means "no filter", so it is the *widest* setting: moving
        // to `["Sent"]` shrinks scope even though the list grew. Caught
        // by `narrowing_labels_does_not_reingest` before this existed.
        let prior = scope_config_blob(&opts(&[], None, named(None)));
        let plan = Adjustments::plan(Some(&prior), &opts(&["Sent"], None, named(None)));
        assert_eq!(plan, Adjustments::default());
    }

    #[test]
    fn removing_the_filter_reingests() {
        // The mirror image: dropping to `[]` admits every label, and the
        // naive set-difference reading would see no addition at all.
        let prior = scope_config_blob(&opts(&["Sent"], None, named(None)));
        assert!(Adjustments::plan(Some(&prior), &opts(&[], None, named(None))).reingest_files);
    }

    #[test]
    fn narrowed_labels_are_a_noop() {
        // The store is already a superset. A hash-based record would
        // re-read the whole export here and produce nothing.
        let prior = scope_config_blob(&opts(&["Sent", "Inbox"], None, named(None)));
        let plan = Adjustments::plan(Some(&prior), &opts(&["Sent"], None, named(None)));
        assert_eq!(plan, Adjustments::default());
    }

    #[test]
    fn relaxed_blob_cap_reingests_files() {
        let prior = scope_config_blob(&opts(&[], Some(1000), named(None)));
        assert!(
            Adjustments::plan(Some(&prior), &opts(&[], Some(5000), named(None))).reingest_files
        );
        assert!(Adjustments::plan(Some(&prior), &opts(&[], None, named(None))).reingest_files);
    }

    #[test]
    fn tightened_blob_cap_is_a_noop() {
        let prior = scope_config_blob(&opts(&[], Some(5000), named(None)));
        let plan = Adjustments::plan(Some(&prior), &opts(&[], Some(1000), named(None)));
        assert_eq!(plan, Adjustments::default());
    }

    // ── the case a hash would get wrong ──────────────────────────────

    #[test]
    fn account_edit_refreshes_without_rereading() {
        // The account row is written by `flush_account_and_lookups`,
        // which never reads a message — so this must cost one UPSERT,
        // not a re-read of the whole export.
        let prior = scope_config_blob(&opts(&["Sent"], None, named(None)));
        let plan = Adjustments::plan(Some(&prior), &opts(&["Sent"], None, named(Some("Work"))));
        assert!(plan.refresh_account);
        assert!(
            !plan.reingest_files,
            "an account-field edit must not re-read the mbox"
        );
    }

    #[test]
    fn account_and_labels_can_both_move() {
        let prior = scope_config_blob(&opts(&["Sent"], None, named(None)));
        let plan = Adjustments::plan(
            Some(&prior),
            &opts(&["Sent", "Inbox"], None, named(Some("Work"))),
        );
        assert!(plan.reingest_files);
        assert!(plan.refresh_account);
    }

    #[test]
    fn blob_shape_is_the_scope_affecting_subset() {
        let obj = scope_config_blob(&opts(&["Sent"], Some(7), named(Some("Work"))));
        let obj = obj.as_object().unwrap();
        assert_eq!(obj.len(), 3, "unexpected keys: {obj:?}");
        assert_eq!(obj[K_ONLY_LABELS], json!(["Sent"]));
        assert_eq!(obj[K_BLOB_CAP], json!(7));
        assert_eq!(obj[K_ACCOUNT]["display_name"], json!("Work"));
    }
}
