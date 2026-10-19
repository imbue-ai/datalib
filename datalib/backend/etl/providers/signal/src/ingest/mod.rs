//! Signal download entry point: every snapshot under the backup folder
//! the store does not hold, oldest first, one dated commit each, then the
//! newest on top. `INGEST.md` beside this crate has the rules.

pub mod backups;
pub mod db;
pub mod schema_raw;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use datalib_etl::bulk::{bulk_upsert_first_seen_in_tx, BulkUpsertable};
use datalib_etl::control::DownloadControl;
use datalib_etl::doltlite_raw as dr;
use datalib_etl::progress::Progress;
use datalib_etl::prune;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl::stop::StopFlag;
use datalib_signal_backup::{backup, decrypt_attachment, local_media_name, Snapshot};
use serde::Serialize;
use sqlx::Row;
use tracing::info;

use backups::{Found, Top, LEDGER};
pub use db::{db_path_for, RawDb};
pub use schema_raw::{AccountRow, ChatItemRow, ChatRow, RecipientRow};

pub const DEFAULT_AEP_ENV: &str = "SIGNAL_BACKUP_PASSPHRASE";

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// The store this run writes into, opened and closed by the caller.
    /// A download never opens a store of its own: one writer per file
    /// (`datalib/backend/etl/README.md` § "One writer per file, by
    /// construction").
    pub db: RawDb,
    /// Directory containing one or more `signal-backup-YYYY-MM-DD-HH-MM-SS/`
    /// snapshot subdirs, every one of which is read.
    pub snapshot_root: PathBuf,
    /// This host's shared fingerprint cache. Host state, so it lives
    /// outside the scan store — see [`datalib_etl_files::fingerprint_cache`].
    pub cache: datalib_etl_files::fingerprint_cache::FingerprintCache,
    /// Directory holding the encrypted attachment blobs (the shared
    /// `files/XX/<media_name>` tree). When `None`, defaults to
    /// `snapshot_root.join("files")` — the layout Signal Android
    /// produces. Override exists so a user can point at a separated
    /// files tree (e.g. on a different volume) without symlinking.
    pub files_root: Option<PathBuf>,
    /// The account entropy pool the backup is encrypted under.
    pub aep: String,
    pub progress: Progress,
    pub control: DownloadControl,
}

#[derive(Debug, Default, Serialize, Clone)]
pub struct FetchSummary {
    /// Snapshots in the folder, whether read this run or before.
    pub backups_found: usize,
    /// The snapshots this run read, in order; the last is what HEAD holds.
    pub backups_read: Vec<String>,
    /// The last snapshot read, and what it held.
    pub snapshot: String,
    /// Blake3 hex of that snapshot (see `schema_raw::SNAPSHOT_BLAKE3_RECIPE_DOC`).
    pub snapshot_blake3: String,
    pub recipients: usize,
    pub chats: usize,
    pub chat_items: usize,
    /// Media file names listed in its `files` sidecar (which catalogs the
    /// shared `files/XX/<name>` tree).
    pub media_files: usize,
    /// Newly-stored attachment blobs (decrypted + landed in the CAS),
    /// across the run.
    pub blobs: usize,
    /// Attachments whose bytes an earlier read already stored, so they
    /// were not decrypted again, across the run.
    pub blobs_skipped: usize,
    /// Attachments we couldn't decrypt/read (file missing, MAC fail,
    /// LocatorInfo without a local key, …), across the run. Each is a
    /// `problems` row on its `chat_item_attachments` edge.
    pub blob_errors: usize,
    /// Rows a snapshot read before held that the next one did not, across
    /// the run.
    pub pruned: usize,
}

/// One snapshot read into the store, not yet committed.
struct Read {
    backup_time_ms: u64,
    /// Frames that would not decode, so it was not read whole.
    undecoded: Vec<String>,
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let pool = opts.db.pool().clone();
    // Not the run's stop flag, which would drop what is reported after a
    // stop: a snapshot is read whole once begun, and the run's stop is
    // honoured between snapshots.
    let never_stops = StopFlag::new();
    run_problems::collecting(&pool, &never_stops, |found| read_backups(opts, found)).await
}

async fn read_backups(opts: FetchOptions, found: RunProblems) -> Result<FetchSummary> {
    let db = &opts.db;
    let aep = &opts.aep;

    // `path` lives inside the `backup` table (not on SourceCommon.input_path),
    // so core's load-time tilde expansion doesn't reach it.
    let snapshot_root = expand_tilde(&opts.snapshot_root);
    let files_root = opts
        .files_root
        .clone()
        .map(|p| expand_tilde(&p))
        .unwrap_or_else(|| snapshot_root.join("files"));

    let (snapshots, unreadable) = find_snapshots(&opts.cache, &snapshot_root).await?;
    let plan = backups::plan(snapshots, &db.read_ledger().await?);
    info!(
        event = "signal_plan",
        root = %snapshot_root.display(),
        found = plan.found.len(),
        to_read = plan.ingest.len(),
        unreadable = unreadable.len(),
        "planned the backup folder"
    );
    if plan.ingest.is_empty() {
        info!(
            event = "signal_snapshot_already_ingested",
            root = %snapshot_root.display(),
            "every snapshot in the folder was ingested before; nothing new to decrypt"
        );
    }
    let mut summary = FetchSummary {
        backups_found: plan.found.len(),
        ..Default::default()
    };
    let mut failed: Vec<String> = Vec::new();
    for (name, why) in unreadable {
        found.record_failed(LEDGER, &name, why);
        failed.push(name);
    }

    let mut last: Option<Read> = None;
    let mut first_failure: Option<String> = None;
    let stop = &opts.control.stop;
    for snap in &plan.ingest {
        if stop.requested() {
            break;
        }
        match read_snapshot(db, aep, &files_root, snap, &mut summary).await? {
            Ok(read) => {
                let date = commit_date(read.backup_time_ms);
                let msg = format!("download signal: backup {}", snap.name);
                dr::commit_run_dated(db.pool(), &msg, date.as_deref()).await?;
                last = Some(read);
            }
            Err(why) => {
                first_failure.get_or_insert_with(|| format!("{}: {why}", snap.name));
                found.record_failed(LEDGER, &snap.name, why);
                failed.push(snap.name.clone());
            }
        }
    }
    // Every snapshot it tried would not read: a passphrase that is not
    // this account's, most likely, and not something to file away as
    // one problem per backup.
    if let (None, Some(first)) = (&last, &first_failure) {
        bail!("no Signal backup this run tried would read; first: {first}");
    }

    if !stop.requested() {
        match backups::top(&plan, &db.read_ledger().await?, &failed) {
            Top::Read(snap) => {
                match read_snapshot(db, aep, &files_root, snap, &mut summary).await? {
                    Ok(read) => {
                        // Dated now: the reason for this commit is now, not
                        // when the backup was taken.
                        let msg = format!(
                            "download signal: backup {}, read again to put the newest back on top",
                            snap.name
                        );
                        dr::commit_run(db.pool(), &msg).await?;
                        last = Some(read);
                    }
                    Err(why) => found.record_failed(LEDGER, &snap.name, why),
                }
            }
            Top::Gone(name) => found.record_failed(
                LEDGER,
                &name,
                "the newest backup the store holds is no longer in the folder, so it cannot \
                 be read again; HEAD stays on the state the last sync left until a newer \
                 backup arrives",
            ),
            Top::Holds | Top::Nothing => {}
        }
        // Every run plans every snapshot, so one not stopped tried every
        // snapshot an earlier run could not read.
        found.records_tried_all(LEDGER);
    }

    match last {
        // What HEAD holds is the last snapshot read, so its report replaces
        // the last one's.
        Some(read) => {
            if let Some(first) = read.undecoded.first() {
                found.phase(
                    "frames",
                    format!(
                        "{} backup frame(s) did not decode, so their records are missing and \
                         nothing this backup dropped was deleted, until a newer backup reads; \
                         first: {first}",
                        read.undecoded.len()
                    ),
                );
            }
        }
        // Nothing was read, so what the last read could not decode stands.
        None => found.cut_short(),
    }
    Ok(summary)
}

/// Every snapshot directory under `root`, fingerprinted, and
/// `(name, why)` for each one that could not be.
async fn find_snapshots(
    cache: &datalib_etl_files::fingerprint_cache::FingerprintCache,
    root: &Path,
) -> Result<(Vec<Found>, Vec<(String, String)>)> {
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(root).with_context(|| format!("read_dir {}", root.display()))? {
        let entry = entry.context("read_dir entry")?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if backups::is_snapshot_name(&name) && entry.file_type().is_ok_and(|t| t.is_dir()) {
            dirs.push((name, entry.path()));
        }
    }
    if dirs.is_empty() {
        bail!("no signal-backup-* subdirectory under {}", root.display());
    }
    let mut found = Vec::new();
    let mut unreadable = Vec::new();
    for (name, dir) in dirs {
        match schema_raw::snapshot_fingerprint(cache, &dir).await {
            Ok(fingerprint) => found.push(Found {
                name,
                dir,
                fingerprint,
            }),
            Err(e) => unreadable.push((name, format!("{e:#}"))),
        }
    }
    Ok((found, unreadable))
}

/// The commit date for a snapshot: when Signal says it was taken.
fn commit_date(backup_time_ms: u64) -> Option<String> {
    let date = i64::try_from(backup_time_ms)
        .ok()
        .filter(|&ms| ms > 0)
        .and_then(chrono::DateTime::from_timestamp_millis)
        .map(|t| t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    if date.is_none() {
        tracing::warn!(
            backup_time_ms,
            "signal: a backup carries no usable time; committing it dated now"
        );
    }
    date
}

/// Read one snapshot into the store's working set and the ledger.
/// `Ok(Err(why))` for one that would not open, before anything is
/// written; an `Err` once writing began fails the run, since the working
/// set may hold half of it.
async fn read_snapshot(
    db: &RawDb,
    aep: &str,
    files_root: &Path,
    snap: &Found,
    summary: &mut FetchSummary,
) -> Result<Result<Read, String>> {
    info!(
        event = "signal_open_snapshot",
        snapshot = %snap.dir.display(),
        files_root = %files_root.display(),
        "opened the backup snapshot"
    );
    let opened = {
        let dir = snap.dir.clone();
        let aep = aep.to_string();
        tokio::task::spawn_blocking(move || -> Result<_> {
            // Streams in 64KiB chunks; tens of MB → a few hundred ms.
            let (blake3, size) = compute_snapshot_blake3(&dir).context("hash snapshot")?;
            // gunzip + AES on tens of MB.
            let snap = Snapshot::open(&dir, &aep).context("decrypt snapshot")?;
            let info = snap.backup_info()?;
            Ok((blake3, size, snap, info))
        })
        .await
        .context("join snapshot read task")?
    };
    let (snapshot_blake3, total_byte_size, snap_data, info) = match opened {
        Ok(o) => o,
        Err(e) => return Ok(Err(format!("{e:#}"))),
    };

    // Entity rows accumulate in memory and go out through the generic
    // bulk upsert at the end — every table by the same path. Attachment
    // bytes are decrypted during the frame walk but their CAS and entity writes
    // are batched through `PendingAttachments`.
    let mut accounts: Vec<AccountRow> = Vec::new();
    let mut recipients: Vec<RecipientRow> = Vec::new();
    let mut chats: Vec<ChatRow> = Vec::new();
    let mut chat_items: Vec<ChatItemRow> = Vec::new();
    let mut pending_attachments = PendingAttachments::default();

    // Pre-load `(media_name → blake3)` for everything decrypted in a prior
    // read, so an attachment shared between two snapshots skips the AES
    // step. One query and O(N) memory, against N per-row queries during
    // the walk. Empty after a blobs reset, so everything re-decrypts.
    let already_decrypted: std::collections::HashMap<String, String> = {
        let rows = sqlx::query(
            "SELECT ref_id, blake3 FROM chat_item_attachments WHERE blake3 IS NOT NULL",
        )
        .fetch_all(db.pool())
        .await
        .context("preload already-decrypted attachments")?;
        rows.iter()
            .map(|r| {
                Ok((
                    r.try_get::<String, _>("ref_id")?,
                    r.try_get::<String, _>("blake3")?,
                ))
            })
            .collect::<Result<_, sqlx::Error>>()
            .context("decode already-decrypted attachments")?
    };

    let mut undecoded: Vec<String> = Vec::new();
    for frame in snap_data.frames() {
        let frame = match frame {
            Ok(f) => f,
            Err(e) => {
                undecoded.push(e.to_string());
                continue;
            }
        };
        match &frame.item {
            Some(backup::frame::Item::Account(a)) => {
                let payload = serde_json::to_string(a).context("serialize account frame")?;
                accounts.push(AccountRow {
                    id_and_payload: datalib_etl::doltlite_raw::WirePayload {
                        id: "self".to_string(),
                        payload,
                    },
                });
            }
            Some(backup::frame::Item::Recipient(r)) => {
                let id = r.id.to_string();
                let (identifier, name) = recipient_pretty(r);
                let payload = serde_json::to_string(r).context("serialize recipient frame")?;
                recipients.push(RecipientRow {
                    id_and_payload: datalib_etl::doltlite_raw::WirePayload { id, payload },
                    identifier,
                    display_name: name,
                });
            }
            Some(backup::frame::Item::Chat(c)) => {
                let id = c.id.to_string();
                let rid = c.recipient_id.to_string();
                let payload = serde_json::to_string(c).context("serialize chat frame")?;
                chats.push(ChatRow {
                    id_and_payload: datalib_etl::doltlite_raw::WirePayload { id, payload },
                    recipient_id: rid,
                });
            }
            Some(backup::frame::Item::ChatItem(ci)) => {
                let chat_id = ci.chat_id.to_string();
                let author_id = ci.author_id.to_string();
                let date_sent = ci.date_sent as i64;
                let pk = schema_raw::chat_item_id_recipe(&chat_id, &author_id, date_sent);

                // Decrypt synchronously (AES-256 can't be batched), then
                // queue the entity row and CAS bytes for the end-of-read
                // flush.
                if let Some(backup::chat_item::Item::StandardMessage(sm)) = &ci.item {
                    for (idx, att) in sm.attachments.iter().enumerate() {
                        ingest_attachment(
                            files_root,
                            &pk,
                            idx,
                            att,
                            &already_decrypted,
                            &mut pending_attachments,
                            summary,
                        );
                    }
                }

                let payload = serde_json::to_string(ci).context("serialize chat_item frame")?;
                chat_items.push(ChatItemRow {
                    id_and_payload: datalib_etl::doltlite_raw::WirePayload { id: pk, payload },
                    chat_id,
                    author_id,
                    date_sent,
                });
            }
            _ => {
                // StickerPack, AdHocCall, NotificationProfile,
                // ChatFolder — not modelled in this first pass.
            }
        }
    }
    summary.snapshot = snap.name.clone();
    summary.snapshot_blake3 = snapshot_blake3.clone();
    summary.recipients = recipients.len();
    summary.chats = chats.len();
    summary.chat_items = chat_items.len();
    summary.media_files = snap_data.file_names().len();

    // The snapshot is the whole store, so read whole it replaces what an
    // older one left, in the transaction that writes it. One with a frame
    // that would not decode was not read whole: what it seems to have
    // dropped may be in that frame, so it deletes nothing.
    //
    // A row's sidecar is stamped the first time a snapshot holds it and
    // left alone after, so a day's commit holds only what that day changed.
    let read_whole = undecoded.is_empty();
    let now = datalib_time::IsoOffsetTimestamp::now_local();
    let mut tx = db.pool().begin().await.context("begin snapshot tx")?;
    bulk_upsert_first_seen_in_tx(&mut tx, &accounts, &now).await?;
    bulk_upsert_first_seen_in_tx(&mut tx, &recipients, &now).await?;
    bulk_upsert_first_seen_in_tx(&mut tx, &chats, &now).await?;
    bulk_upsert_first_seen_in_tx(&mut tx, &chat_items, &now).await?;
    if read_whole {
        let kept = [
            (AccountRow::TABLE, ids_of(&accounts)),
            (RecipientRow::TABLE, ids_of(&recipients)),
            (ChatRow::TABLE, ids_of(&chats)),
            (ChatItemRow::TABLE, ids_of(&chat_items)),
            (
                schema_raw::ChatItemAttachmentRow::TABLE,
                ids_of(&pending_attachments.rows),
            ),
        ];
        for (table, keep) in &kept {
            let gone = prune::prune_scope_in_tx(&mut tx, table, &[], keep).await?;
            prune::record(table, keep.len() + gone.len(), gone.len());
            summary.pruned += gone.len();
        }
    }
    tx.commit().await.context("commit snapshot tx")?;
    // Attachment bulk-flush: one CAS-pool tx (`put_many`) + one
    // entity-pool tx (chat_item_attachments + bookkeeping + per-row
    // error annotations), committed with the rows above.
    flush_attachments(db, pending_attachments).await?;

    db.record_snapshot_ingested(
        &snap.fingerprint,
        &snapshot_blake3,
        &snap.dir.to_string_lossy(),
        total_byte_size,
    )
    .await
    .context("record snapshot in ingested_backups")?;
    summary.backups_read.push(snap.name.clone());

    Ok(Ok(Read {
        backup_time_ms: info.backup_time_ms,
        undecoded,
    }))
}

fn ids_of<T: BulkUpsertable>(rows: &[T]) -> HashSet<String> {
    rows.iter().map(|r| r.id().to_string()).collect()
}

fn compute_snapshot_blake3(snapshot_dir: &Path) -> Result<(String, u64)> {
    use std::io::Read;
    let mut hasher = blake3::Hasher::new();
    let mut total: u64 = 0;
    for name in ["metadata", "main", "files"] {
        let path = snapshot_dir.join(name);
        let mut f = std::fs::File::open(&path)
            .with_context(|| format!("open {} for hashing", path.display()))?;
        let mut buf = [0u8; 64 * 1024];
        loop {
            let n = f
                .read(&mut buf)
                .with_context(|| format!("read {}", path.display()))?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            total += n as u64;
        }
    }
    Ok((hasher.finalize().to_hex().to_string(), total))
}

/// Accumulator threaded through the frame walk. Per-attachment work
/// (decrypt + hash) happens during the walk; the actual CAS +
/// entity-table writes are batched and flushed once at the end of
/// [`fetch`]. See [`flush_attachments`] for the flush.
#[derive(Default)]
struct PendingAttachments {
    /// One row per attempted attachment slot (success + failure
    /// alike). A row whose bytes are in `cas_items` gets its `blake3`
    /// from the CAS at the flush; a failure (file missing, decrypt
    /// failed) keeps `blake3 = None` + an `errors` entry below.
    rows: Vec<schema_raw::ChatItemAttachmentRow>,
    /// Plaintext bytes ready for the CAS, with the index of their row
    /// in `rows` and their content type. One entry per successful
    /// attachment.
    /// Decrypted bytes can be large; we accumulate them in memory
    /// only between the frame walk and the end-of-fetch flush, then
    /// drop them — no per-row buffer outlives the flush.
    cas_items: Vec<DecryptedCas>,
    /// Per-row error messages to record in
    /// `chat_item_attachments_bookkeeping` after the entity-table
    /// bulk flush. Keyed by the attachment row id.
    errors: Vec<datalib_etl::blob_cas::BlobNotFetched>,
}

struct DecryptedCas {
    row: usize,
    content_type: Option<String>,
    bytes: Vec<u8>,
}

/// Decrypt one `MessageAttachment`'s bytes out of the shared
/// `<files_root>/XX/<media_name>` tree and queue them for batched
/// CAS + entity-table writes via the `PendingAttachments`
/// accumulator.
#[allow(clippy::too_many_arguments)]
fn ingest_attachment(
    files_root: &Path,
    chat_item_pk: &str,
    slot_idx: usize,
    att: &backup::MessageAttachment,
    already_decrypted: &std::collections::HashMap<String, String>,
    pending: &mut PendingAttachments,
    summary: &mut FetchSummary,
) {
    let attachment_id = schema_raw::chat_item_attachment_id_recipe(chat_item_pk, slot_idx);
    let (plaintext_hash, local_key) = match locate(att) {
        Ok(found) => found,
        // Nothing to retry: the bytes are not in this backup. The edge
        // carries the attachment's own id for a ref, having no media name.
        Err(why) => {
            pending.rows.push(schema_raw::ChatItemAttachmentRow {
                id: attachment_id.clone(),
                chat_item_id: chat_item_pk.to_string(),
                ref_id: attachment_id.clone(),
                blake3: None,
            });
            pending.errors.push(datalib_etl::blob_cas::BlobNotFetched {
                ref_id: attachment_id,
                detail: format!("the backup does not carry this attachment's bytes: {why}"),
                reason: datalib_problems::Reason::NotFound,
            });
            summary.blob_errors += 1;
            return;
        }
    };
    let media_name = local_media_name(&plaintext_hash, &local_key);

    // Skip-check: if a prior run already decrypted this media_name
    // (anywhere in any chat_item), we know the blake3 without
    // re-decrypting. The CAS already has the bytes (a store reset
    // leaves `cas_objects` alone), so we just record the new
    // (chat_item_id, slot) → blake3 edge.
    if let Some(blake3) = already_decrypted.get(&media_name) {
        pending.rows.push(schema_raw::ChatItemAttachmentRow {
            id: attachment_id,
            chat_item_id: chat_item_pk.to_string(),
            ref_id: media_name,
            blake3: Some(blake3.clone()),
        });
        summary.blobs_skipped += 1;
        return;
    }

    let shard = &media_name[..2];
    let enc_path = files_root.join(shard).join(&media_name);
    let enc = match std::fs::read(&enc_path) {
        Ok(b) => b,
        Err(e) => {
            pending.rows.push(schema_raw::ChatItemAttachmentRow {
                id: attachment_id.clone(),
                chat_item_id: chat_item_pk.to_string(),
                ref_id: media_name,
                blake3: None,
            });
            pending.errors.push(blob_failed(
                attachment_id,
                format!("read {}: {e}", enc_path.display()),
            ));
            summary.blob_errors += 1;
            return;
        }
    };

    let plaintext = match decrypt_attachment(&enc, &local_key) {
        Ok(p) => p,
        Err(e) => {
            pending.rows.push(schema_raw::ChatItemAttachmentRow {
                id: attachment_id.clone(),
                chat_item_id: chat_item_pk.to_string(),
                ref_id: media_name,
                blake3: None,
            });
            pending
                .errors
                .push(blob_failed(attachment_id, format!("decrypt: {e}")));
            summary.blob_errors += 1;
            return;
        }
    };

    pending.cas_items.push(DecryptedCas {
        row: pending.rows.len(),
        content_type: att.pointer.as_ref().and_then(|p| p.content_type.clone()),
        bytes: plaintext,
    });
    pending.rows.push(schema_raw::ChatItemAttachmentRow {
        id: attachment_id,
        chat_item_id: chat_item_pk.to_string(),
        ref_id: media_name,
        blake3: None,
    });
    summary.blobs += 1;
}

/// The plaintext hash and local key an attachment's bytes are found and
/// decrypted by, or why the backup has none. Signal leaves them out for
/// an attachment the device never downloaded.
fn locate(att: &backup::MessageAttachment) -> Result<(Vec<u8>, [u8; 64]), &'static str> {
    let ptr = att.pointer.as_ref().ok_or("no file pointer")?;
    let li = ptr.locator_info.as_ref().ok_or("no locator")?;
    let local_key_bytes = li.local_key.as_deref().ok_or("no local key")?;
    let local_key: [u8; 64] = local_key_bytes
        .try_into()
        .map_err(|_| "a local key that is not 64 bytes")?;
    match li.integrity_check.as_ref() {
        Some(backup::file_pointer::locator_info::IntegrityCheck::PlaintextHash(h))
            if !h.is_empty() =>
        {
            Ok((h.clone(), local_key))
        }
        _ => Err("no plaintext hash"),
    }
}

/// End-of-fetch flush: CAS `put_many` for newly-decrypted bytes, each
/// row taking the key its bytes were stored under, then the shared
/// [`datalib_etl::blob_cas::flush_cas_edges`] (bulk UPSERT
/// `chat_item_attachments` → bookkeeping `last_error` stamps).
async fn flush_attachments(db: &RawDb, pending: PendingAttachments) -> Result<()> {
    use datalib_etl::blob_cas::CasInsert;
    let PendingAttachments {
        mut rows,
        cas_items,
        errors,
    } = pending;
    let cas_inserts: Vec<CasInsert<'_, usize>> = cas_items
        .iter()
        .map(|c| CasInsert {
            id: c.row,
            bytes: &c.bytes,
            content_type: c.content_type.as_deref(),
        })
        .collect();
    for (row, key) in db.cas().put_many(cas_inserts).await? {
        rows[row].blake3 = Some(key);
    }
    datalib_etl::blob_cas::flush_cas_edges(db.pool(), rows, &errors).await
}

/// A blob this provider could not read or decrypt. Signal has no
/// deliberate skips, so every one of its blob problems is a failure.
fn blob_failed(ref_id: String, detail: String) -> datalib_etl::blob_cas::BlobNotFetched {
    datalib_etl::blob_cas::BlobNotFetched {
        ref_id,
        detail,
        reason: datalib_problems::Reason::FetchFailed,
    }
}

fn recipient_pretty(r: &backup::Recipient) -> (Option<String>, Option<String>) {
    use backup::recipient::Destination;
    match r.destination.as_ref() {
        Some(Destination::Self_(_)) => (Some("me".into()), Some("Me".into())),
        Some(Destination::Contact(c)) => {
            let identifier = match (c.e164, c.aci.as_ref(), c.pni.as_ref()) {
                (Some(n), _, _) if n != 0 => Some(format!("+{n}")),
                (_, Some(a), _) if !a.is_empty() => Some(hex_lower(a)),
                (_, _, Some(p)) if !p.is_empty() => Some(hex_lower(p)),
                _ => None,
            };
            // Best-effort display: prefer profile name; fall back to
            // system name. Matches what `dump.py` surfaces in practice.
            let name = c
                .profile_given_name
                .as_ref()
                .filter(|s| !s.is_empty())
                .map(|g| {
                    let family = c.profile_family_name.as_deref().unwrap_or("");
                    format!("{g} {family}").trim().to_string()
                })
                .or_else(|| {
                    let g = &c.system_given_name;
                    let f = &c.system_family_name;
                    if g.is_empty() && f.is_empty() {
                        None
                    } else {
                        Some(format!("{g} {f}").trim().to_string())
                    }
                });
            (identifier, name)
        }
        _ => (None, None),
    }
}

fn expand_tilde(p: &Path) -> PathBuf {
    if let Ok(rest) = p.strip_prefix("~") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    p.to_path_buf()
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}
