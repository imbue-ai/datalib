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

use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use frankweiler_etl::control::ExtractControl;
use frankweiler_etl::progress::Progress;
use frankweiler_signal_backup::{backup, Snapshot};
use prost::Message;
use serde::Serialize;
use tracing::{info, warn};

pub use db::{db_path_for, RawDb};

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
    pub snapshot: String,
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
    info!(
        event = "signal_open_snapshot",
        snapshot = %snapshot_dir.display()
    );

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
        ..Default::default()
    };

    for frame in snap.frames() {
        let frame = match frame {
            Ok(f) => f,
            Err(e) => {
                warn!(event = "signal_frame_decode_error", error = %e);
                continue;
            }
        };
        let raw = frame.encode_to_vec();
        match frame.item {
            Some(backup::frame::Item::Account(_)) => {
                db.upsert_account(&raw).await?;
            }
            Some(backup::frame::Item::Recipient(r)) => {
                let id = r.id.to_string();
                let (identifier, name) = recipient_pretty(&r);
                db.upsert_recipient(&id, identifier.as_deref(), name.as_deref(), &raw)
                    .await?;
                summary.recipients += 1;
            }
            Some(backup::frame::Item::Chat(c)) => {
                let id = c.id.to_string();
                let rid = c.recipient_id.to_string();
                db.upsert_chat(&id, &rid, &raw).await?;
                summary.chats += 1;
            }
            Some(backup::frame::Item::ChatItem(ci)) => {
                let chat_id = ci.chat_id.to_string();
                let author_id = ci.author_id.to_string();
                let date_sent = ci.date_sent as i64;
                let pk = format!("{chat_id}#{author_id}#{date_sent}");
                db.upsert_chat_item(&pk, &chat_id, &author_id, date_sent, &raw)
                    .await?;
                summary.chat_items += 1;
            }
            _ => {
                // StickerPack, AdHocCall, NotificationProfile,
                // ChatFolder — not modelled in this first pass.
            }
        }
    }

    Ok(summary)
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
