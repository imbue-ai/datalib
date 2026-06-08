//! End-to-end test against a real Signal backup. Skipped silently if
//! `SIGNAL_BACKUP_AEP` or `SIGNAL_BACKUP_SNAPSHOT` aren't in the env,
//! so `bazel test //...` stays green for everyone. Under Bazel this
//! test is also `tags = ["manual"]` to belt-and-suspenders avoid
//! incidental runs without the env wired in.
//!
//! Run locally with:
//!
//!   SIGNAL_BACKUP_AEP=...secret... \
//!   SIGNAL_BACKUP_SNAPSHOT=~/backups/SignalBackups/signal-backup-... \
//!   bazel test //frankweiler/backend/signal-backup:signal_backup_e2e \
//!     --test_env=SIGNAL_BACKUP_AEP \
//!     --test_env=SIGNAL_BACKUP_SNAPSHOT
//!
//! Never log the AEP — only counts.

// Tests print diagnostics to stderr for the manual `bazel test
// --test_output=streamed` flow; the workspace's library-level lint
// against `eprintln!` doesn't apply to integration-test code.
#![allow(clippy::disallowed_macros)]

use std::path::Path;

use frankweiler_signal_backup::{backup, decrypt_attachment, Snapshot};

#[test]
fn open_real_snapshot_and_decrypt_one_attachment() {
    let (aep, snapshot) = match (
        std::env::var("SIGNAL_BACKUP_AEP").ok(),
        std::env::var("SIGNAL_BACKUP_SNAPSHOT").ok(),
    ) {
        (Some(a), Some(s)) if !a.is_empty() && !s.is_empty() => (a, s),
        _ => {
            eprintln!("SIGNAL_BACKUP_{{AEP,SNAPSHOT}} not set — skipping e2e");
            return;
        }
    };
    let snapshot_path = Path::new(&snapshot);
    let snap = Snapshot::open(snapshot_path, &aep).expect("open snapshot");

    let mut counts = Counts::default();
    let mut first_attachment: Option<([u8; 64], Vec<u8>)> = None;

    for frame in snap.frames() {
        let frame = frame.expect("decode frame");
        match frame.item {
            Some(backup::frame::Item::Account(_)) => counts.account += 1,
            Some(backup::frame::Item::Recipient(_)) => counts.recipient += 1,
            Some(backup::frame::Item::Chat(_)) => counts.chat += 1,
            Some(backup::frame::Item::ChatItem(ci)) => {
                counts.chat_item += 1;
                if first_attachment.is_none() {
                    first_attachment = first_attachment_key(&ci);
                }
            }
            _ => {}
        }
    }

    eprintln!(
        "frames: account={} recipient={} chat={} chat_item={}; files={}",
        counts.account,
        counts.recipient,
        counts.chat,
        counts.chat_item,
        snap.file_names().len(),
    );

    assert!(counts.recipient > 0, "expected ≥1 recipient");
    assert!(counts.chat_item > 0, "expected ≥1 chat item");
    assert!(!snap.file_names().is_empty(), "expected ≥1 media file name");

    let (lk, ph) = match first_attachment {
        Some(v) => v,
        None => {
            eprintln!("no attachments in frames — skipping attachment decrypt");
            return;
        }
    };
    let media_name = frankweiler_signal_backup::local_media_name(&ph, &lk);
    let files_root = match std::env::var("SIGNAL_BACKUP_FILES_ROOT").ok() {
        Some(p) => std::path::PathBuf::from(p),
        None => snapshot_path
            .parent()
            .expect("snapshot has parent")
            .join("files"),
    };
    let shard = &media_name[..2];
    let enc_path = files_root.join(shard).join(&media_name);
    if !enc_path.exists() {
        eprintln!(
            "attachment {} not found at {} — skipping decrypt",
            media_name,
            enc_path.display()
        );
        return;
    }
    let enc = std::fs::read(&enc_path).expect("read enc attachment");
    let pt = decrypt_attachment(&enc, &lk).expect("decrypt attachment");
    assert!(!pt.is_empty(), "decrypted attachment is empty");
    eprintln!(
        "decrypted attachment {} bytes -> {} bytes",
        enc.len(),
        pt.len()
    );
}

#[derive(Default)]
struct Counts {
    account: usize,
    recipient: usize,
    chat: usize,
    chat_item: usize,
}

/// Pull `(local_key, plaintext_hash)` out of the first attachment we
/// find on a chat item — checks both standardMessage attachments and
/// stickerMessage.sticker.data.
fn first_attachment_key(item: &backup::ChatItem) -> Option<([u8; 64], Vec<u8>)> {
    use backup::chat_item;
    match item.item.as_ref()? {
        chat_item::Item::StandardMessage(sm) => {
            for att in &sm.attachments {
                if let Some(ptr) = att.pointer.as_ref() {
                    if let Some(k) = pointer_local_key(ptr) {
                        return Some(k);
                    }
                }
            }
            None
        }
        chat_item::Item::StickerMessage(sm) => {
            let s = sm.sticker.as_ref()?;
            let ptr = s.data.as_ref()?;
            pointer_local_key(ptr)
        }
        _ => None,
    }
}

fn pointer_local_key(ptr: &backup::FilePointer) -> Option<([u8; 64], Vec<u8>)> {
    let li = ptr.locator_info.as_ref()?;
    let lk = li.local_key.as_ref()?;
    if lk.len() != 64 {
        return None;
    }
    let ph = match li.integrity_check.as_ref()? {
        backup::file_pointer::locator_info::IntegrityCheck::PlaintextHash(h) if !h.is_empty() => {
            h.clone()
        }
        _ => return None,
    };
    let mut k = [0u8; 64];
    k.copy_from_slice(lk);
    Some((k, ph))
}
