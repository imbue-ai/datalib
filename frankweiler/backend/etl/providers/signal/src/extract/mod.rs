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
use frankweiler_etl::blob_cas::RefStub;
use frankweiler_etl::control::ExtractControl;
use frankweiler_etl::progress::Progress;
use frankweiler_signal_backup::{backup, decrypt_attachment, local_media_name, Snapshot};
use serde::Serialize;
use tracing::{info, warn};

pub use db::{db_path_for, ChatItemRow, ChatRow, RawDb, RecipientRow};

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
    /// log lines; details land on `blob_refs_bookkeeping.last_error`.
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
        // Signal doesn't extract attachments into the CAS yet, but
        // the flag flows through uniformly so the day attachment
        // ingest lands no wiring is needed.
        frankweiler_etl::doltlite_raw::truncate_blob_refs(db.pool()).await?;
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

    // Accumulate entity rows in memory and bulk-upsert in chunked
    // multi-row INSERTs at the end. Mirror of email's mbox bulk
    // pattern — see `docs/data_architecture_ingestion.md` §
    // "Bulk-upsert as the standard write path". Attachments still
    // ingest per-row inside the frame loop because each one needs a
    // skip-check + decrypt + CAS write that aren't bulk-shaped today.
    let mut recipients: Vec<RecipientRow> = Vec::new();
    let mut chats: Vec<ChatRow> = Vec::new();
    let mut chat_items: Vec<ChatItemRow> = Vec::new();

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
                db.upsert_account(&payload).await?;
            }
            Some(backup::frame::Item::Recipient(r)) => {
                let id = r.id.to_string();
                let (identifier, name) = recipient_pretty(r);
                let payload = serde_json::to_string(r).context("serialize recipient frame")?;
                recipients.push(RecipientRow {
                    id,
                    identifier,
                    display_name: name,
                    payload,
                });
                summary.recipients += 1;
            }
            Some(backup::frame::Item::Chat(c)) => {
                let id = c.id.to_string();
                let rid = c.recipient_id.to_string();
                let payload = serde_json::to_string(c).context("serialize chat frame")?;
                chats.push(ChatRow {
                    id,
                    recipient_id: rid,
                    payload,
                });
                summary.chats += 1;
            }
            Some(backup::frame::Item::ChatItem(ci)) => {
                let chat_id = ci.chat_id.to_string();
                let author_id = ci.author_id.to_string();
                let date_sent = ci.date_sent as i64;
                let pk = schema_raw::chat_item_id_recipe(&chat_id, &author_id, date_sent);

                // Walk attachments + push their bytes into the CAS.
                // Pattern matches every other media-bearing provider:
                // `db.store_blob(&RefStub { .. }, &bytes)` with a
                // skip-check that lets re-extracts stay cheap.
                if let Some(backup::chat_item::Item::StandardMessage(sm)) = &ci.item {
                    for (idx, att) in sm.attachments.iter().enumerate() {
                        ingest_attachment(&db, &files_root, &pk, idx, att, &mut summary).await?;
                    }
                }

                let payload = serde_json::to_string(ci).context("serialize chat_item frame")?;
                chat_items.push(ChatItemRow {
                    id: pk,
                    chat_id,
                    author_id,
                    date_sent,
                    payload,
                });
                summary.chat_items += 1;
            }
            _ => {
                // StickerPack, AdHocCall, NotificationProfile,
                // ChatFolder — not modelled in this first pass.
            }
        }
    }

    // One tx per entity table, each a chunked multi-row INSERT with
    // bookkeeping inside.
    db.bulk_upsert_recipients(&recipients).await?;
    db.bulk_upsert_chats(&chats).await?;
    db.bulk_upsert_chat_items(&chat_items).await?;

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

/// Decrypt one `MessageAttachment`'s bytes out of the shared
/// `<files_root>/XX/<media_name>` tree and stash them in the CAS.
///
/// Quietly does nothing when the attachment doesn't carry the
/// fields we need to locate it on disk (no `LocatorInfo`, no
/// `local_key`, integrity check is `encrypted_digest` rather than
/// `plaintext_hash`, …). Signal's wire format permits all of those
/// states for valid attachments — they just mean we don't have the
/// local plaintext to surface, so the Translate pass renders the
/// message text without an inline link.
async fn ingest_attachment(
    db: &RawDb,
    files_root: &Path,
    chat_item_pk: &str,
    slot_idx: usize,
    att: &backup::MessageAttachment,
    summary: &mut FetchSummary,
) -> Result<()> {
    let Some(ptr) = att.pointer.as_ref() else {
        return Ok(());
    };
    let Some(li) = ptr.locator_info.as_ref() else {
        return Ok(());
    };
    let Some(local_key_bytes) = li.local_key.as_deref() else {
        return Ok(());
    };
    if local_key_bytes.len() != 64 {
        return Ok(());
    }
    let plaintext_hash = match li.integrity_check.as_ref() {
        Some(backup::file_pointer::locator_info::IntegrityCheck::PlaintextHash(h))
            if !h.is_empty() =>
        {
            h.clone()
        }
        _ => return Ok(()),
    };
    let mut local_key = [0u8; 64];
    local_key.copy_from_slice(local_key_bytes);

    let media_name = local_media_name(&plaintext_hash, &local_key);
    let slot_str = slot_idx.to_string();

    // Skip-check: same shape every other provider uses. The
    // `blob_refs.blake3 IS NOT NULL` lookup keyed on `ref_id` makes
    // re-extracts a no-op for already-ingested attachments — see
    // `docs/ETL_general_shape.md` "Blobs and the CAS split".
    if db.blob_exists(&media_name).await.unwrap_or(false) {
        summary.blobs_skipped += 1;
        return Ok(());
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
            let _ = db
                .record_blob_error(
                    &media_name,
                    chat_item_pk,
                    &slot_str,
                    &format!("read {}: {e}", enc_path.display()),
                )
                .await;
            summary.blob_errors += 1;
            return Ok(());
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
            let _ = db
                .record_blob_error(
                    &media_name,
                    chat_item_pk,
                    &slot_str,
                    &format!("decrypt: {e}"),
                )
                .await;
            summary.blob_errors += 1;
            return Ok(());
        }
    };

    db.store_blob(
        &RefStub {
            ref_id: &media_name,
            kind: "signal_attachment",
            owning_id: chat_item_pk,
            slot: &slot_str,
            upstream_uuid: Some(&media_name),
            upstream_name: ptr.file_name.as_deref(),
            source_url: None,
            content_type: ptr.content_type.as_deref(),
        },
        &plaintext,
    )
    .await?;
    summary.blobs += 1;
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
