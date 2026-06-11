//! Signal extract entry point.
//!
//! Discovers the latest `signal-backup-*` snapshot under
//! `opts.snapshot_root`, decrypts it with the AEP read from
//! `opts.aep_env_var` (default `SIGNAL_PASSPHRASE`), iterates frames,
//! and UPSERTs them into the doltlite raw store. One backup snapshot
//! per fetch — older snapshots are ignored; cleaning them up is the
//! user's problem.
//!
//! The AEP never lands on disk: we read it from the env at call time,
//! pass it through the [`Snapshot::open`] derivation, and drop it.

pub mod db;
pub mod schema_raw;

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use frankweiler_etl::bulk::bulk_upsert_in_tx;
use frankweiler_etl::control::ExtractControl;
use frankweiler_etl::progress::Progress;
use frankweiler_signal_backup::{backup, decrypt_attachment, local_media_name, Snapshot};
use serde::Serialize;
use sqlx::Row;
use tracing::{info, warn};

pub use db::{db_path_for, RawDb};
pub use schema_raw::{AccountRow, ChatItemRow, ChatRow, RecipientRow};

const DEFAULT_AEP_ENV: &str = "SIGNAL_PASSPHRASE";

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Doltlite database path. Ignored when `db` is `Some`.
    pub db_path: PathBuf,
    /// Pre-opened raw DB (sync orchestrator populates this).
    pub db: Option<RawDb>,
    /// Directory containing one or more `signal-backup-YYYY-MM-DD-HH-MM-SS/`
    /// snapshot subdirs. The newest (lexicographically — Signal's
    /// timestamps sort correctly) is the one we ingest.
    pub snapshot_root: PathBuf,
    /// Directory holding the encrypted attachment blobs (the shared
    /// `files/XX/<media_name>` tree). When `None`, defaults to
    /// `snapshot_root.join("files")` — the layout Signal Android
    /// produces. Override exists so a user can point at a separated
    /// files tree (e.g. on a different volume) without symlinking.
    pub files_root: Option<PathBuf>,
    /// Name of the env var holding the AEP. Defaults to
    /// `SIGNAL_PASSPHRASE`. Letting the user override means a single
    /// process can keep AEPs for multiple Signal accounts segregated
    /// at the shell level (`SIGNAL_PASSPHRASE_PERSONAL`, …).
    pub aep_env_var: Option<String>,
    pub progress: Progress,
    pub control: ExtractControl,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            db_path: PathBuf::new(),
            db: None,
            snapshot_root: PathBuf::new(),
            files_root: None,
            aep_env_var: None,
            progress: Progress::noop(),
            control: ExtractControl::default(),
        }
    }
}

#[derive(Debug, Default, Serialize, Clone)]
pub struct FetchSummary {
    pub recipients: usize,
    pub chats: usize,
    pub chat_items: usize,
    /// Number of media file names listed in the snapshot's `files`
    /// sidecar (which catalogs the shared `files/XX/<name>` tree).
    pub media_files: usize,
    /// Newly-stored attachment blobs (decrypted + landed in the CAS).
    pub blobs: usize,
    /// Attachments whose ref already had a hash attached, so we
    /// short-circuited without touching disk.
    pub blobs_skipped: usize,
    /// Attachments we couldn't decrypt/read (file missing, MAC fail,
    /// LocatorInfo without a local key, …). Surfaces as warn-level
    /// log lines; details land on
    /// `chat_item_attachments_bookkeeping.last_error`.
    pub blob_errors: usize,
    pub snapshot: String,
    /// Blake3 hex of the snapshot (see `schema_raw::SNAPSHOT_BLAKE3_RECIPE_DOC`).
    pub snapshot_blake3: String,
    /// True when fetch short-circuited because this snapshot was
    /// already recorded in `ingested_backups`. When true, all other
    /// counters are zero.
    pub already_ingested: bool,
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db = match opts.db.clone() {
        Some(db) => db,
        None => RawDb::open(&db_path_for(&opts.db_path)).await?,
    };
    if opts.control.reset_and_redownload {
        db.reset().await?;
    }
    if opts.control.refetch_blobs {
        // Wipe the attachment edge table + its bookkeeping so the
        // next walk re-decrypts every attachment. `cas_objects`
        // itself is never wiped — re-decrypted bytes hash to the
        // same blake3 and the `INSERT OR IGNORE` on the CAS side
        // is a no-op. This is the Signal-specific equivalent of the
        // shared `truncate_blob_refs` (which Signal no longer uses).
        sqlx::query("DELETE FROM chat_item_attachments")
            .execute(db.pool())
            .await
            .context("truncate chat_item_attachments")?;
        sqlx::query("DELETE FROM chat_item_attachments_bookkeeping")
            .execute(db.pool())
            .await
            .context("truncate chat_item_attachments_bookkeeping")?;
    }

    let aep_env_var = opts
        .aep_env_var
        .clone()
        .unwrap_or_else(|| DEFAULT_AEP_ENV.to_string());
    let aep = std::env::var(&aep_env_var).map_err(|_| {
        anyhow!(
            "${aep_env_var} not set — pass the Signal AEP via that env var (sourced from .envrc.private etc.)"
        )
    })?;

    // `snapshot_dir` lives inside the `sync:` block (not on
    // SourceCommon.input_path), so core's load-time tilde expansion
    // doesn't reach it. Expand here for the convenience of YAML that
    // says `snapshot_dir: ~/backups/SignalBackups`.
    let snapshot_root = expand_tilde(&opts.snapshot_root);
    let snapshot_dir = pick_latest_snapshot(&snapshot_root)
        .with_context(|| format!("pick latest snapshot under {}", snapshot_root.display()))?;
    let files_root = opts
        .files_root
        .clone()
        .map(|p| expand_tilde(&p))
        .unwrap_or_else(|| snapshot_root.join("files"));
    info!(
        event = "signal_open_snapshot",
        snapshot = %snapshot_dir.display(),
        files_root = %files_root.display(),
    );

    // Resume cursor — fast path. Build a stat-derived fingerprint
    // (three `(mtime_ns, byte_size)` pairs joined by `:`) and look
    // it up against `ingested_backups`. No body I/O on the skip,
    // no crypto, no decrypt. See `schema_raw::snapshot_fingerprint`.
    let fingerprint = schema_raw::snapshot_fingerprint(&snapshot_dir)
        .with_context(|| format!("snapshot fingerprint {}", snapshot_dir.display()))?;
    if db.snapshot_already_ingested(&fingerprint).await? {
        info!(
            event = "signal_snapshot_already_ingested",
            snapshot = %snapshot_dir.display(),
            fingerprint = %fingerprint,
            note = "skipping decrypt + walk; pass --reset-and-redownload to re-ingest",
        );
        return Ok(FetchSummary {
            snapshot: snapshot_dir
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_default(),
            snapshot_blake3: String::new(),
            already_ingested: true,
            ..Default::default()
        });
    }

    // Fingerprint missed. Compute the forensic blake3 (used to fill
    // the `blake3` column on a successful record) before we open the
    // decrypted snapshot. Streams in 64KiB chunks; tens of MB → a
    // few hundred ms.
    let snapshot_dir_for_hash = snapshot_dir.clone();
    let (snapshot_blake3, total_byte_size) =
        tokio::task::spawn_blocking(move || compute_snapshot_blake3(&snapshot_dir_for_hash))
            .await
            .context("join snapshot hash task")?
            .with_context(|| format!("hash snapshot {}", snapshot_dir.display()))?;

    // Heavy crypto work — gunzip + AES on tens of MB — runs in a
    // blocking thread so we don't block the tokio runtime.
    let snap = {
        let snapshot_dir = snapshot_dir.clone();
        tokio::task::spawn_blocking(move || Snapshot::open(&snapshot_dir, &aep))
            .await
            .context("join snapshot decrypt task")?
            .context("decrypt snapshot")?
    };

    let mut summary = FetchSummary {
        media_files: snap.file_names().len(),
        snapshot: snapshot_dir
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default(),
        snapshot_blake3: snapshot_blake3.clone(),
        ..Default::default()
    };

    // Accumulate entity rows in memory and bulk-upsert via the
    // generic `bulk_upsert_in_tx` helper at the end. Every table
    // (including singleton `account` and the new attachment table)
    // goes through the same code path — see
    // `docs/data_architecture_ingestion.md` §"One writer per row"
    // and §"Bulk-upsert as the standard write path". Attachment
    // bytes are decrypted during the frame walk (per-attachment
    // AES-256) but the CAS + entity-table writes are batched via
    // `PendingAttachments` and flushed once at the end.
    let mut accounts: Vec<AccountRow> = Vec::new();
    let mut recipients: Vec<RecipientRow> = Vec::new();
    let mut chats: Vec<ChatRow> = Vec::new();
    let mut chat_items: Vec<ChatItemRow> = Vec::new();
    let mut pending_attachments = PendingAttachments::default();

    // Skip-check map: pre-load `(media_name → blake3)` for every
    // attachment we have already decrypted in a prior run. Lets us
    // skip the AES decrypt step for media files that appear in this
    // snapshot but were already processed in an earlier one (common
    // when two snapshots share an unchanged photo). One query,
    // O(N) memory, vs. N per-row queries during the walk. After
    // `--refetch-blobs` the table is empty so the map is empty and
    // every attachment gets re-decrypted.
    let already_decrypted: std::collections::HashMap<String, String> = {
        let rows = sqlx::query(
            "SELECT ref_id, blake3 FROM chat_item_attachments WHERE blake3 IS NOT NULL",
        )
        .fetch_all(db.pool())
        .await
        .context("preload already-decrypted attachments")?;
        rows.iter()
            .filter_map(|r| {
                let ref_id: String = r.try_get("ref_id").ok()?;
                let blake3: String = r.try_get("blake3").ok()?;
                Some((ref_id, blake3))
            })
            .collect()
    };

    for frame in snap.frames() {
        let frame = match frame {
            Ok(f) => f,
            Err(e) => {
                warn!(event = "signal_frame_decode_error", error = %e);
                continue;
            }
        };
        match &frame.item {
            Some(backup::frame::Item::Account(a)) => {
                let payload = serde_json::to_string(a).context("serialize account frame")?;
                let payload_blake3 = frankweiler_etl::blob_cas::blake3_hex(payload.as_bytes());
                accounts.push(AccountRow {
                    triad: frankweiler_etl::doltlite_raw::WirePayloadTriad {
                        id: "self".to_string(),
                        payload,
                        payload_blake3,
                    },
                });
            }
            Some(backup::frame::Item::Recipient(r)) => {
                let id = r.id.to_string();
                let (identifier, name) = recipient_pretty(r);
                let payload = serde_json::to_string(r).context("serialize recipient frame")?;
                let payload_blake3 = frankweiler_etl::blob_cas::blake3_hex(payload.as_bytes());
                recipients.push(RecipientRow {
                    triad: frankweiler_etl::doltlite_raw::WirePayloadTriad {
                        id,
                        payload,
                        payload_blake3,
                    },
                    identifier,
                    display_name: name,
                });
                summary.recipients += 1;
            }
            Some(backup::frame::Item::Chat(c)) => {
                let id = c.id.to_string();
                let rid = c.recipient_id.to_string();
                let payload = serde_json::to_string(c).context("serialize chat frame")?;
                let payload_blake3 = frankweiler_etl::blob_cas::blake3_hex(payload.as_bytes());
                chats.push(ChatRow {
                    triad: frankweiler_etl::doltlite_raw::WirePayloadTriad {
                        id,
                        payload,
                        payload_blake3,
                    },
                    recipient_id: rid,
                });
                summary.chats += 1;
            }
            Some(backup::frame::Item::ChatItem(ci)) => {
                let chat_id = ci.chat_id.to_string();
                let author_id = ci.author_id.to_string();
                let date_sent = ci.date_sent as i64;
                let pk = schema_raw::chat_item_id_recipe(&chat_id, &author_id, date_sent);

                // Walk attachments: decrypt synchronously (AES-256
                // can't be batched), then queue the entity-row +
                // CAS bytes into `PendingAttachments` for the
                // end-of-fetch bulk flush. Re-extracts skip cheaply
                // via the new `chat_item_attachments` table's
                // `(ref_id, blake3)` index — see
                // [`schema_raw::CHAT_ITEM_ATTACHMENTS_DDL`].
                if let Some(backup::chat_item::Item::StandardMessage(sm)) = &ci.item {
                    for (idx, att) in sm.attachments.iter().enumerate() {
                        ingest_attachment(
                            &files_root,
                            &pk,
                            idx,
                            att,
                            &already_decrypted,
                            &mut pending_attachments,
                            &mut summary,
                        );
                    }
                }

                let payload = serde_json::to_string(ci).context("serialize chat_item frame")?;
                let payload_blake3 = frankweiler_etl::blob_cas::blake3_hex(payload.as_bytes());
                chat_items.push(ChatItemRow {
                    triad: frankweiler_etl::doltlite_raw::WirePayloadTriad {
                        id: pk,
                        payload,
                        payload_blake3,
                    },
                    chat_id,
                    author_id,
                    date_sent,
                });
                summary.chat_items += 1;
            }
            _ => {
                // StickerPack, AdHocCall, NotificationProfile,
                // ChatFolder — not modelled in this first pass.
            }
        }
    }

    // One generic UPSERT path per table — same call, different row
    // type. All four batches land in their own tx; an inner crash
    // never leaves a half-applied snapshot because the snapshot-level
    // commit happens in the orchestrator (see §"Commit lifecycle").
    let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    {
        let mut tx = db.pool().begin().await.context("begin account tx")?;
        bulk_upsert_in_tx(&mut tx, &accounts, &now).await?;
        tx.commit().await.context("commit account tx")?;
    }
    {
        let mut tx = db.pool().begin().await.context("begin recipients tx")?;
        bulk_upsert_in_tx(&mut tx, &recipients, &now).await?;
        tx.commit().await.context("commit recipients tx")?;
    }
    {
        let mut tx = db.pool().begin().await.context("begin chats tx")?;
        bulk_upsert_in_tx(&mut tx, &chats, &now).await?;
        tx.commit().await.context("commit chats tx")?;
    }
    {
        let mut tx = db.pool().begin().await.context("begin chat_items tx")?;
        bulk_upsert_in_tx(&mut tx, &chat_items, &now).await?;
        tx.commit().await.context("commit chat_items tx")?;
    }
    // Attachment bulk-flush: one CAS-pool tx (`put_many`) + one
    // entity-pool tx (chat_item_attachments + bookkeeping + per-row
    // error annotations).
    flush_attachments(&db, pending_attachments).await?;

    db.record_snapshot_ingested(
        &fingerprint,
        &snapshot_blake3,
        &snapshot_dir.to_string_lossy(),
        total_byte_size,
    )
    .await
    .context("record snapshot in ingested_backups")?;

    Ok(summary)
}

/// Hash the three on-disk files of a Signal snapshot directory
/// (`metadata || main || files`) into a single Blake3 hex string,
/// alongside the total byte count.
///
/// See `schema_raw::SNAPSHOT_BLAKE3_RECIPE_DOC` for the canonical
/// statement of the recipe. Streams each file in 64 KiB chunks so we
/// don't materialize the whole thing in memory — `main` can be tens
/// of MB.
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
    /// alike). On success carries `blake3 = Some(hex)`; on
    /// failure (file missing, decrypt failed) carries
    /// `blake3 = None` + an `errors` entry below.
    rows: Vec<schema_raw::ChatItemAttachmentRow>,
    /// Plaintext bytes ready for the CAS, paired with their hash
    /// and content_type. One entry per successful attachment.
    /// Decrypted bytes can be large; we accumulate them in memory
    /// only between the frame walk and the end-of-fetch flush, then
    /// drop them — no per-row buffer outlives the flush.
    cas_items: Vec<DecryptedCas>,
    /// Per-row error messages to record in
    /// `chat_item_attachments_bookkeeping` after the entity-table
    /// bulk flush. Keyed by the attachment row id.
    errors: Vec<(String, String)>,
}

struct DecryptedCas {
    blake3: String,
    content_type: Option<String>,
    bytes: Vec<u8>,
}

/// Decrypt one `MessageAttachment`'s bytes out of the shared
/// `<files_root>/XX/<media_name>` tree and queue them for batched
/// CAS + entity-table writes via the `PendingAttachments`
/// accumulator.
///
/// Quietly does nothing when the attachment doesn't carry the
/// fields we need to locate it on disk (no `LocatorInfo`, no
/// `local_key`, integrity check is `encrypted_digest` rather than
/// `plaintext_hash`, …). Signal's wire format permits all of those
/// states for valid attachments — they just mean we don't have the
/// local plaintext to surface, so the Translate pass renders the
/// message text without an inline link.
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
    let Some(ptr) = att.pointer.as_ref() else {
        return;
    };
    let Some(li) = ptr.locator_info.as_ref() else {
        return;
    };
    let Some(local_key_bytes) = li.local_key.as_deref() else {
        return;
    };
    if local_key_bytes.len() != 64 {
        return;
    }
    let plaintext_hash = match li.integrity_check.as_ref() {
        Some(backup::file_pointer::locator_info::IntegrityCheck::PlaintextHash(h))
            if !h.is_empty() =>
        {
            h.clone()
        }
        _ => return,
    };
    let mut local_key = [0u8; 64];
    local_key.copy_from_slice(local_key_bytes);

    let media_name = local_media_name(&plaintext_hash, &local_key);
    let attachment_id = schema_raw::chat_item_attachment_id_recipe(chat_item_pk, slot_idx);

    // Skip-check: if a prior run already decrypted this media_name
    // (anywhere in any chat_item), we know the blake3 without
    // re-decrypting. The CAS already has the bytes
    // (`cas_objects` survives `--reset-and-redownload`), so we
    // just record the new (chat_item_id, slot) → blake3 edge.
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
            warn!(
                event = "signal_attachment_missing",
                media_name = %media_name,
                path = %enc_path.display(),
                error = %e,
            );
            pending.rows.push(schema_raw::ChatItemAttachmentRow {
                id: attachment_id.clone(),
                chat_item_id: chat_item_pk.to_string(),
                ref_id: media_name,
                blake3: None,
            });
            pending
                .errors
                .push((attachment_id, format!("read {}: {e}", enc_path.display())));
            summary.blob_errors += 1;
            return;
        }
    };

    let plaintext = match decrypt_attachment(&enc, &local_key) {
        Ok(p) => p,
        Err(e) => {
            warn!(
                event = "signal_attachment_decrypt_failed",
                media_name = %media_name,
                error = %e,
            );
            pending.rows.push(schema_raw::ChatItemAttachmentRow {
                id: attachment_id.clone(),
                chat_item_id: chat_item_pk.to_string(),
                ref_id: media_name,
                blake3: None,
            });
            pending
                .errors
                .push((attachment_id, format!("decrypt: {e}")));
            summary.blob_errors += 1;
            return;
        }
    };

    let blake3 = frankweiler_etl::blob_cas::blake3_hex(&plaintext);
    pending.rows.push(schema_raw::ChatItemAttachmentRow {
        id: attachment_id,
        chat_item_id: chat_item_pk.to_string(),
        ref_id: media_name,
        blake3: Some(blake3.clone()),
    });
    pending.cas_items.push(DecryptedCas {
        blake3,
        content_type: ptr.content_type.clone(),
        bytes: plaintext,
    });
    summary.blobs += 1;
}

/// End-of-fetch flush. One CAS-pool tx (`put_many`) + one entity-pool
/// tx (chunked multi-row UPSERT + bookkeeping) + per-row error
/// recording. Order: CAS first so the entity-table row's `blake3`
/// points at bytes that are definitely already in the CAS.
async fn flush_attachments(db: &RawDb, pending: PendingAttachments) -> Result<()> {
    if pending.rows.is_empty() {
        return Ok(());
    }
    use frankweiler_etl::blob_cas::CasInsert;
    use frankweiler_etl::bulk::bulk_upsert_in_tx;

    // CAS pool: one chunked INSERT OR IGNORE across all decrypted
    // bytes. No-op for attachments that failed before decrypt.
    let cas_inserts: Vec<CasInsert<'_>> = pending
        .cas_items
        .iter()
        .map(|c| CasInsert {
            blake3: &c.blake3,
            bytes: &c.bytes,
            content_type: c.content_type.as_deref(),
        })
        .collect();
    db.cas().put_many(&cas_inserts).await?;

    // Entity pool: bulk UPSERT the attachment rows (success +
    // failure alike) + bulk bookkeeping. Then walk the per-row error
    // list and stamp `last_error` on the relevant bookkeeping rows.
    let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    {
        let mut tx = db
            .pool()
            .begin()
            .await
            .context("begin chat_item_attachments tx")?;
        bulk_upsert_in_tx(&mut tx, &pending.rows, &now).await?;
        // Error stamps: `record_object_attempt` with `Some(err)`
        // updates bookkeeping.last_error and bumps attempt_count
        // again — same shape every per-row error path uses. We do
        // these inside the same tx so a failure here doesn't leave
        // entity rows without their error annotations.
        for (id, err) in &pending.errors {
            frankweiler_etl::doltlite_raw::record_object_attempt(
                &mut tx,
                "chat_item_attachments",
                id,
                Some(err),
            )
            .await?;
        }
        tx.commit()
            .await
            .context("commit chat_item_attachments tx")?;
    }
    Ok(())
}

/// Pick the newest `signal-backup-*` subdir under `root`. Signal's
/// dirname format is `signal-backup-YYYY-MM-DD-HH-MM-SS`, which sorts
/// lexicographically the same as chronologically.
fn pick_latest_snapshot(root: &Path) -> Result<PathBuf> {
    let mut best: Option<(String, PathBuf)> = None;
    let entries =
        std::fs::read_dir(root).with_context(|| format!("read_dir {}", root.display()))?;
    for entry in entries {
        let entry = entry.context("read_dir entry")?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("signal-backup-") {
            continue;
        }
        if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            continue;
        }
        let path = entry.path();
        match &best {
            Some((b, _)) if b.as_str() >= name.as_ref() => {}
            _ => best = Some((name.into_owned(), path)),
        }
    }
    best.map(|(_, p)| p)
        .ok_or_else(|| anyhow!("no signal-backup-* subdirectory under {}", root.display()))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn picks_latest_snapshot_by_name() {
        let tmp = TempDir::new().unwrap();
        for n in [
            "signal-backup-2026-05-01-10-00-00",
            "signal-backup-2026-06-08-20-27-22",
            "signal-backup-2026-02-15-08-15-00",
            "random-unrelated-dir",
        ] {
            fs::create_dir(tmp.path().join(n)).unwrap();
        }
        let got = pick_latest_snapshot(tmp.path()).unwrap();
        assert!(got.ends_with("signal-backup-2026-06-08-20-27-22"));
    }

    #[test]
    fn errors_when_no_snapshot_present() {
        let tmp = TempDir::new().unwrap();
        fs::create_dir(tmp.path().join("random")).unwrap();
        assert!(pick_latest_snapshot(tmp.path()).is_err());
    }
}
