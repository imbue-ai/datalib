//! Apple Mail: one account of the Mail app on this Mac, read from
//! `~/Library/Mail` into the shared email raw store as if it had come off
//! a JMAP server. Mail's Envelope Index says which messages the account
//! has, where each is filed and its flags; each message's bytes are its
//! `.emlx` file, with any attachments Mail keeps beside a `.partial.emlx`
//! put back. How Mail's folder is laid out:
//! `docs/dev/email_download_modes.md` §6.

pub mod emlx;
mod index;
pub mod plan;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use datalib_etl::blob_cas::blake3_hex;
use datalib_etl::control::DownloadControl;
use datalib_etl::download_problems::{RecordProblem, RunProblem, SkippedRecord};
use datalib_etl::progress::Progress;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl_files::file_changes_scan::{self, ListedFile, ScanOptions, ScannedFile};
use datalib_etl_files::file_checkpoint;
use datalib_etl_files::fingerprint_cache::FingerprintCache;
use serde::Serialize;

use super::db::RawDb;
use super::envelope;
use super::mbox::{bulk_insert_mailboxes, bulk_insert_threads, write_batch, PendingBatch};
use super::schema_raw::{AccountRow, FetchedEmail, SEEN};

/// The cursor scope: which `.emlx` and attachment files this source has
/// read, and what each hashed to.
const SCOPE: &str = "email/apple_mail";

/// Emails held in memory before a write.
const FLUSH_BATCH: usize = 1000;

/// Emails between two moves of the bar: one event each would be tens of
/// thousands of them for one account.
const PROGRESS_EVERY: usize = 250;

/// The source's `apple_mail` block, as the download needs it.
#[derive(Debug, Clone, Default)]
pub struct AppleMailAccountConfig {
    /// Mail's id for the account; with none, the one account holding mail.
    pub account: Option<String>,
    /// The account every email is filed under in the store.
    pub account_id: Option<String>,
    pub display_name: Option<String>,
    pub email_address: Option<String>,
    pub is_personal: Option<bool>,
}

pub struct FetchOptions {
    /// The store this run writes into, opened and closed by the caller.
    pub db: RawDb,
    /// `~/Library/Mail`, a version folder inside it (`V10`), or a copy of
    /// either.
    pub path: PathBuf,
    pub account: AppleMailAccountConfig,
    pub only_labels: Vec<String>,
    pub whole_threads: bool,
    pub cache: FingerprintCache,
    pub progress: Progress,
    pub control: DownloadControl,
}

#[derive(Debug, Default, Serialize, Clone)]
pub struct FetchSummary {
    pub mailboxes_upserted: usize,
    pub threads_upserted: usize,
    pub emails_upserted: usize,
    /// Emails whose file and filing were what the store already held.
    pub emails_unchanged: usize,
    /// Messages in Mail's index with no file yet: Mail has not
    /// downloaded them.
    pub emails_without_file: usize,
    /// Emails stored with an attachment Mail has not downloaded.
    pub emails_missing_attachments: usize,
    pub emails_removed: usize,
    pub mailboxes_removed: usize,
    pub read_errors: usize,
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let (pool, stop) = (opts.db.pool().clone(), opts.control.stop.clone());
    run_problems::collecting(&pool, &stop, |found| read_account(opts, found)).await
}

/// The folder Mail keeps its version's data in: the one holding
/// `MailData/Envelope Index`, which is `path` itself or its newest `V<n>`.
pub fn version_dir(path: &Path) -> Result<PathBuf> {
    let has_index = |dir: &Path| index_path(dir).is_file();
    if has_index(path) {
        return Ok(path.to_path_buf());
    }
    let entries = std::fs::read_dir(path).with_context(|| {
        format!(
            "list {} (macOS lets an app read Mail's folder only once it is chosen in a file \
             dialog, or the app has Full Disk Access)",
            path.display()
        )
    })?;
    let mut versions: Vec<(u32, PathBuf)> = entries
        .filter_map(|e| e.ok())
        .filter_map(|e| {
            let name = e.file_name();
            let n: u32 = name.to_str()?.strip_prefix('V')?.parse().ok()?;
            Some((n, e.path()))
        })
        .filter(|(_, p)| has_index(p))
        .collect();
    versions.sort();
    versions.pop().map(|(_, p)| p).ok_or_else(|| {
        anyhow!(
            "{} is not Apple Mail's folder: no `V<n>/MailData/Envelope Index` in it. \
             Mail keeps its mail in ~/Library/Mail.",
            path.display()
        )
    })
}

fn index_path(version_dir: &Path) -> PathBuf {
    version_dir.join("MailData").join("Envelope Index")
}

/// The message a file under a version folder belongs to: an `.emlx`
/// (`…/Messages/<rowid>.emlx`, `….partial.emlx`) or a part Mail keeps for
/// one (`…/Attachments/<rowid>/<part>/<name>`).
#[derive(Debug, Clone, PartialEq, Eq)]
enum FileOf {
    Message { rowid: i64, partial: bool },
    Part { rowid: i64, part: String },
}

fn file_of(rel: &str) -> Option<FileOf> {
    let parts: Vec<&str> = rel.split('/').collect();
    let n = parts.len();
    if n >= 2 && parts[n - 2] == "Messages" {
        let name = parts[n - 1];
        let (stem, partial) = match name.strip_suffix(".partial.emlx") {
            Some(stem) => (stem, true),
            None => (name.strip_suffix(".emlx")?, false),
        };
        return Some(FileOf::Message {
            rowid: stem.parse().ok()?,
            partial,
        });
    }
    if n >= 4 && parts[n - 4] == "Attachments" {
        return Some(FileOf::Part {
            rowid: parts[n - 3].parse().ok()?,
            part: parts[n - 2].to_string(),
        });
    }
    None
}

/// Where each message's bytes are, from a listing.
#[derive(Default)]
struct Files<'a> {
    /// A message's `.emlx`: the whole one where both are there.
    message: HashMap<i64, &'a ListedFile>,
    /// The parts Mail keeps for a message: part number → its files.
    parts: HashMap<i64, BTreeMap<String, Vec<&'a ListedFile>>>,
}

impl<'a> Files<'a> {
    fn from_listing(files: &'a [ListedFile]) -> Self {
        let mut out = Files::default();
        for f in files {
            match file_of(&f.rel) {
                Some(FileOf::Message { rowid, partial }) => {
                    if !partial || !out.message.contains_key(&rowid) {
                        out.message.insert(rowid, f);
                    }
                }
                Some(FileOf::Part { rowid, part }) => out
                    .parts
                    .entry(rowid)
                    .or_default()
                    .entry(part)
                    .or_default()
                    .push(f),
                None => {}
            }
        }
        out
    }

    fn of(&self, rowid: i64) -> Vec<&'a ListedFile> {
        self.message
            .get(&rowid)
            .copied()
            .into_iter()
            .chain(
                self.parts
                    .get(&rowid)
                    .into_iter()
                    .flatten()
                    .flat_map(|(_, f)| f.iter().copied()),
            )
            .collect()
    }
}

/// An email as the store holds it, for telling whether it changed.
#[derive(Debug, PartialEq, Eq)]
struct Held {
    thread_id: String,
    mailbox_ids: Vec<String>,
    /// Every keyword but `$seen`, which is [`Self::read`].
    keywords: Vec<String>,
    read: bool,
}

impl Held {
    /// What the store holds for `planned` once it is written.
    fn of(planned: &plan::PlannedEmail) -> Option<Held> {
        Some(Held {
            thread_id: planned.thread_id.clone()?,
            mailbox_ids: planned.mailbox_ids.clone(),
            keywords: planned
                .keywords
                .iter()
                .filter(|k| *k != SEEN)
                .cloned()
                .collect(),
            read: planned.keywords.iter().any(|k| k == SEEN),
        })
    }
}

async fn held_emails(db: &RawDb, account_id: &str) -> Result<HashMap<String, Held>> {
    let rows: Vec<(String, String)> =
        sqlx::query_as("SELECT id, thread_id FROM emails WHERE account_id = ?")
            .bind(account_id)
            .fetch_all(db.pool())
            .await
            .context("list the account's emails")?;
    let joins = db.load_email_joins().await?;
    let read: HashSet<String> = sqlx::query_scalar("SELECT id FROM read_marks WHERE read = 1")
        .fetch_all(db.pool())
        .await
        .context("list the read marks")?
        .into_iter()
        .collect();
    Ok(rows
        .into_iter()
        .map(|(id, thread_id)| {
            let sorted = |m: &HashMap<String, Vec<String>>| {
                let mut v = m.get(&id).cloned().unwrap_or_default();
                v.sort();
                v.dedup();
                v
            };
            let held = Held {
                thread_id,
                mailbox_ids: sorted(&joins.mailboxes),
                keywords: sorted(&joins.keywords),
                read: read.contains(&id),
            };
            (id, held)
        })
        .collect())
}

fn default_account_id(uuid: &str) -> String {
    let short: String = uuid
        .chars()
        .filter(char::is_ascii_alphanumeric)
        .take(8)
        .collect();
    format!("apple_mail_{}", short.to_ascii_lowercase())
}

async fn read_account(opts: FetchOptions, found: RunProblems) -> Result<FetchSummary> {
    let db = opts.db.clone();
    let root = version_dir(&opts.path)?;
    let rows = index::read(&index_path(&root)).await?;
    let accounts = plan::accounts(&rows);
    let account = plan::choose_account(&accounts, opts.account.account.as_deref())?;
    let account_id = opts
        .account
        .account_id
        .clone()
        .unwrap_or_else(|| default_account_id(&account.uuid));
    let plan = plan::plan(
        &rows,
        &account.uuid,
        &account_id,
        &opts.only_labels,
        opts.whole_threads,
    );
    drop(rows);
    found.config(unmatched_labels(&opts.only_labels, &plan.names));

    let scan_opts = ScanOptions {
        ignore: vec!["/MailData/".to_string()],
        progress: opts.progress.clone(),
        ..ScanOptions::default()
    };
    let listing = file_changes_scan::list(&opts.cache, &root, &scan_opts, |p| {
        p.to_str()
            .is_some_and(|s| s.ends_with(".emlx") || s.contains("/Attachments/"))
    })
    .await?;
    listing.report_problems(&found, "Apple Mail");
    let files = Files::from_listing(&listing.files);

    let planned_rowids: HashSet<i64> = plan
        .emails
        .iter()
        .flat_map(|e| e.rowids.iter().copied())
        .collect();
    let prev = file_checkpoint::load_cursor(db.pool(), SCOPE).await?;
    let changes = listing.changes_since(&prev, |rel| {
        file_of(rel).is_some_and(|f| match f {
            FileOf::Message { rowid, .. } | FileOf::Part { rowid, .. } => {
                planned_rowids.contains(&rowid)
            }
        })
    });
    let may_have_changed: HashSet<&str> =
        changes.to_read().iter().map(|f| f.rel.as_str()).collect();

    let held = held_emails(&db, &account_id).await?;
    let known_blobs = db.loaded_blob_ids().await?;
    let mut summary = FetchSummary::default();
    let mut batch = PendingBatch::default();
    let mut read_files: Vec<ScannedFile> = Vec::new();
    // Every email the account has, filter or not: narrowing
    // `only_extract_labels` never deletes.
    let mut kept: BTreeSet<String> = plan.unfiltered_ids.clone();
    let mut threads: BTreeMap<String, Vec<(String, String)>> = BTreeMap::new();
    let mut unread: Vec<SkippedRecord> = Vec::new();
    // Attachments Mail has not downloaded, per email. Only an email read
    // this run has a verdict; one skipped as unchanged keeps its row.
    let mut missing_parts: Vec<RecordProblem> = Vec::new();
    let mut tried: HashSet<String> = HashSet::new();
    let mut stopped = false;

    opts.progress.set_length(Some(plan.emails.len() as u64));
    let mut done = 0usize;
    for planned in &plan.emails {
        if opts.control.stop.requested() {
            stopped = true;
            break;
        }
        done += 1;
        if done.is_multiple_of(PROGRESS_EVERY) {
            opts.progress.inc(PROGRESS_EVERY as u64);
        }
        let Some((rowid, message_file)) = planned
            .rowids
            .iter()
            .find_map(|r| files.message.get(r).map(|f| (*r, *f)))
        else {
            summary.emails_without_file += 1;
            continue;
        };
        let its_files = files.of(rowid);
        let same_filing = match (&planned.email_id, Held::of(planned)) {
            (Some(id), Some(expected)) => held.get(id) == Some(&expected),
            _ => false,
        };
        if same_filing
            && !its_files
                .iter()
                .any(|f| may_have_changed.contains(f.rel.as_str()))
        {
            summary.emails_unchanged += 1;
            if let Some(id) = &planned.email_id {
                threads
                    .entry(planned.thread_id.clone().unwrap_or_else(|| id.clone()))
                    .or_default()
                    .push((received_key(planned.date_received), id.clone()));
            }
            continue;
        }

        let read = match read_message(&listing, message_file, &files, rowid).await {
            Ok(r) => r,
            Err(e) => {
                summary.read_errors += 1;
                unread.push(SkippedRecord {
                    entry: message_file.rel.clone(),
                    problem: datalib_problems::Problem::record(
                        datalib_problems::Reason::Undeserializable,
                        &format!("{e:#}"),
                    ),
                });
                continue;
            }
        };
        let bytes_as_held = read
            .files
            .iter()
            .all(|f| prev.get(&f.rel) == Some(&f.blake3));
        read_files.extend(read.files.iter().cloned());
        let msg = match envelope::parse(&read.raw) {
            Ok(m) => m,
            Err(e) => {
                summary.read_errors += 1;
                unread.push(SkippedRecord {
                    entry: message_file.rel.clone(),
                    problem: datalib_problems::Problem::record(
                        datalib_problems::Reason::Undeserializable,
                        &format!("{e:#}"),
                    ),
                });
                continue;
            }
        };
        let blob_id = blake3_hex(&read.raw);
        let email_id = planned.email_id.clone().unwrap_or_else(|| blob_id.clone());
        let thread_id = planned
            .thread_id
            .clone()
            .unwrap_or_else(|| email_id.clone());
        kept.insert(email_id.clone());
        tried.insert(email_id.clone());
        if !read.missing.is_empty() {
            summary.emails_missing_attachments += 1;
            missing_parts.push(RecordProblem {
                severity: datalib_problems::Severity::Warning,
                ..RecordProblem::new(
                    "emails",
                    &email_id,
                    format!(
                        "Mail has not downloaded parts {} of {}",
                        read.missing.join(", "),
                        message_file.rel
                    ),
                )
            });
        }
        threads
            .entry(thread_id.clone())
            .or_default()
            .push((received_key(planned.date_received), email_id.clone()));
        if same_filing && bytes_as_held {
            summary.emails_unchanged += 1;
            continue;
        }
        if !known_blobs.contains_key(&blob_id) && batch.seen_blob_ids.insert(blob_id.clone()) {
            batch.cas.add_fetched(
                &email_id,
                &blob_id,
                read.raw.clone(),
                Some("message/rfc822".to_string()),
            );
        }
        let envelope = envelope::synthesize(
            &read.raw,
            &msg,
            &envelope::TransportFacts {
                email_id: email_id.clone(),
                blob_id,
                thread_id,
                mailbox_ids: planned.mailbox_ids.clone(),
                keywords: planned.keywords.clone(),
                preview: None,
            },
        );
        if let Some(email) = FetchedEmail::from_jmap_envelope(&account_id, &envelope) {
            batch.marks.push(email.mark());
            batch.emails.push(email.row);
        }
        if batch.emails.len() >= FLUSH_BATCH {
            summary.emails_upserted += flush(&db, &mut batch, &mut read_files).await?;
        }
    }
    opts.progress.inc((done % PROGRESS_EVERY) as u64);
    summary.emails_upserted += flush(&db, &mut batch, &mut read_files).await?;
    listing.finish().await?;

    write_account_and_lookups(
        &db,
        &account_id,
        &opts.account,
        &plan,
        &threads,
        &mut summary,
    )
    .await?;

    if summary.emails_without_file > 0 {
        found.push(not_downloaded(
            "apple_mail_files",
            format!(
                "{} messages in Mail's index have no file yet: Mail has not downloaded them",
                summary.emails_without_file
            ),
        ));
    }
    // An email the account still has and this run did not read keeps its
    // row; one Mail no longer has was tried, and its row goes.
    let untried: HashSet<String> = plan
        .unfiltered_ids
        .iter()
        .filter(|id| !tried.contains(*id))
        .cloned()
        .collect();
    found.records_failed(missing_parts);
    found.records_tried_all_but("emails", move |id| untried.contains(id));
    found.skipped("apple_mail", unread);

    let walked_clean = listing.errors.is_empty() && listing.unreadable.is_empty();
    if stopped {
        return Ok(summary);
    }
    if walked_clean && summary.read_errors == 0 {
        summary.emails_removed = db.prune_emails_to(&account_id, &kept).await?;
        let gone = changes.gone_by_path(&BTreeSet::new());
        file_checkpoint::forget_files(db.pool(), SCOPE, &gone).await?;
    } else {
        found.push(RunProblem::listing(
            "removed_records",
            "not every file of Mail's folder read cleanly, so no stored email was deleted",
        ));
    }
    Ok(summary)
}

/// Something Mail has not downloaded yet. A warning: nothing failed, and
/// the next sync after Mail has it copies it.
fn not_downloaded(name: &str, detail: String) -> RunProblem {
    RunProblem {
        severity: datalib_problems::Severity::Warning,
        ..RunProblem::phase(name, detail)
    }
}

/// Sorts as the instant does, with an email of no date first.
fn received_key(date: Option<i64>) -> String {
    format!("{:020}", date.unwrap_or(0).max(0))
}

struct ReadMessage {
    raw: Vec<u8>,
    missing: Vec<String>,
    files: Vec<ScannedFile>,
}

async fn read_message(
    listing: &file_changes_scan::Listing,
    message_file: &ListedFile,
    files: &Files<'_>,
    rowid: i64,
) -> Result<ReadMessage> {
    let emlx = listing.read(message_file).await?;
    let mut read = vec![emlx.file];
    let message = emlx::message(&emlx.bytes)?;
    let mut parts: HashMap<String, Vec<u8>> = HashMap::new();
    for (part, part_files) in files.parts.get(&rowid).into_iter().flatten() {
        if let [one] = part_files.as_slice() {
            let bytes = listing.read(one).await?;
            read.push(bytes.file);
            parts.insert(part.clone(), bytes.bytes);
        } else {
            bail!(
                "Mail keeps {} files for part {part}, where it keeps one",
                part_files.len()
            );
        }
    }
    let rebuilt = emlx::rebuild(message, |part| parts.get(part).cloned())?;
    Ok(ReadMessage {
        raw: rebuilt.raw,
        missing: rebuilt.missing,
        files: read,
    })
}

async fn flush(db: &RawDb, batch: &mut PendingBatch, read: &mut Vec<ScannedFile>) -> Result<usize> {
    let n = write_batch(db, batch).await?;
    if !read.is_empty() {
        let mut tx = db.pool().begin().await.context("begin cursor tx")?;
        for f in read.drain(..) {
            file_checkpoint::record_file(&mut tx, SCOPE, &f).await?;
        }
        tx.commit().await.context("commit cursor tx")?;
    }
    Ok(n)
}

async fn write_account_and_lookups(
    db: &RawDb,
    account_id: &str,
    config: &AppleMailAccountConfig,
    plan: &plan::Plan,
    threads: &BTreeMap<String, Vec<(String, String)>>,
    summary: &mut FetchSummary,
) -> Result<()> {
    let now = datalib_time::IsoOffsetTimestamp::now_local();
    let mut tx = db.pool().begin().await.context("begin lookups tx")?;
    let display_name = config
        .display_name
        .clone()
        .unwrap_or_else(|| account_id.to_string());
    let account_row = AccountRow::from_mbox_config(
        account_id,
        Some(display_name.as_str()),
        config.email_address.as_deref(),
        config.is_personal.unwrap_or(true),
    );
    datalib_etl::bulk::bulk_upsert_first_seen_in_tx(&mut tx, &[account_row], &now).await?;

    let mailbox_specs: Vec<(String, String, Option<&'static str>, String)> = plan
        .mailboxes
        .iter()
        .map(|m| {
            let mut payload = serde_json::json!({"id": m.id, "name": m.name});
            if let Some(role) = m.role {
                payload["role"] = serde_json::json!(role);
            }
            (m.id.clone(), m.name.clone(), m.role, payload.to_string())
        })
        .collect();
    bulk_insert_mailboxes(&mut tx, account_id, &mailbox_specs).await?;
    datalib_etl::bulk::bulk_stamp_first_seen(
        &mut tx,
        "mailboxes",
        mailbox_specs.iter().map(|(id, ..)| id.as_str()),
        &now,
    )
    .await?;
    summary.mailboxes_upserted = mailbox_specs.len();

    let thread_specs: Vec<(String, i64, Option<i64>, String)> = threads
        .iter()
        .map(|(id, members)| {
            let mut ordered = members.clone();
            ordered.sort();
            ordered.dedup();
            let ids: Vec<&str> = ordered.iter().map(|(_, id)| id.as_str()).collect();
            let upstream = plan.in_account.get(id).copied();
            let mut payload = serde_json::json!({"id": id, "emailIds": ids});
            if let Some(upstream) = upstream {
                payload["upstreamEmailCount"] = serde_json::json!(upstream);
            }
            (id.clone(), ids.len() as i64, upstream, payload.to_string())
        })
        .collect();
    bulk_insert_threads(&mut tx, account_id, &thread_specs).await?;
    datalib_etl::bulk::bulk_stamp_first_seen(
        &mut tx,
        "threads",
        thread_specs.iter().map(|(id, ..)| id.as_str()),
        &now,
    )
    .await?;
    summary.threads_upserted = thread_specs.len();
    tx.commit().await.context("commit lookups tx")?;

    // A mailbox Mail no longer has comes off its mail.
    let planned: HashSet<&str> = plan.mailboxes.iter().map(|m| m.id.as_str()).collect();
    let gone: Vec<(String, Option<String>)> = db
        .mailbox_names(account_id)
        .await?
        .into_keys()
        .filter(|id| !planned.contains(id.as_str()))
        .map(|id| (id, None))
        .collect();
    summary.mailboxes_removed = gone.len();
    super::refile_mailboxes(db, &now, &gone).await?;
    Ok(())
}

fn unmatched_labels(
    configured: &[String],
    names: &BTreeSet<String>,
) -> Vec<datalib_etl::download_problems::DownloadProblem> {
    configured
        .iter()
        .map(|l| l.trim())
        .filter(|l| !names.contains(*l))
        .map(|l| {
            datalib_etl::download_problems::DownloadProblem::not_found(
                "only_extract_labels",
                l,
                "the Mail account has no mailbox of this name",
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn files_are_tied_to_their_message() {
        let base = "ACC/[Gmail].mbox/All Mail.mbox/STORE/Data";
        assert_eq!(
            file_of(&format!("{base}/Messages/693.emlx")),
            Some(FileOf::Message {
                rowid: 693,
                partial: false
            })
        );
        assert_eq!(
            file_of(&format!("{base}/7/2/Messages/27141.partial.emlx")),
            Some(FileOf::Message {
                rowid: 27141,
                partial: true
            })
        );
        assert_eq!(
            file_of(&format!("{base}/Attachments/141/1.3/invite.ics")),
            Some(FileOf::Part {
                rowid: 141,
                part: "1.3".into()
            })
        );
        assert_eq!(file_of("ACC/INBOX.mbox/Info.plist"), None);
        assert_eq!(file_of(&format!("{base}/Messages/notes.emlx")), None);
    }

    #[test]
    fn the_version_folder_is_found_inside_mails_folder() {
        let dir = tempfile::tempdir().unwrap();
        for v in ["V9", "V10"] {
            let data = dir.path().join(v).join("MailData");
            std::fs::create_dir_all(&data).unwrap();
            std::fs::write(data.join("Envelope Index"), b"").unwrap();
        }
        std::fs::create_dir_all(dir.path().join("V11")).unwrap();
        assert_eq!(version_dir(dir.path()).unwrap(), dir.path().join("V10"));
        assert_eq!(
            version_dir(&dir.path().join("V9")).unwrap(),
            dir.path().join("V9")
        );
        let err = version_dir(&dir.path().join("V11"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not Apple Mail's folder"), "{err}");
    }
}
