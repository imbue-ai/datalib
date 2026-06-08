//! `signal-make-fixture <spec.json> <out_dir>` — produces an
//! encrypted Signal-Android snapshot directory from a TNG-themed
//! JSON spec. AEP defaults to `"0" * 64` (the fixture AEP) so the
//! extractor sees a real backup it can decrypt with a known-public
//! passphrase.
//!
//! The output is byte-stable across runs: every random byte (IVs +
//! backup_id) is held to a deterministic value, so the genrule
//! caches cleanly under Bazel and golden tests are stable.
//!
//! Spec shape:
//!
//! ```jsonc
//! {
//!   "aep": "0000…",                        // 64 chars; optional, default all zeros
//!   "snapshot_dir_name": "signal-backup-…",// optional, default fixed string
//!   "backup_time_ms": 1701000000000,        // BackupInfo.backup_time_ms
//!   "recipients": [
//!     { "id": 1, "self": true, "name": "Captain Jean-Luc Picard" },
//!     { "id": 2, "name": "Will Riker",  "e164": 17015551111 },
//!     …
//!   ],
//!   "chats": [{ "id": 100, "recipient_id": 1 }],
//!   "chat_items": [
//!     { "chat_id": 100, "author_id": 1, "date_sent": 1700000000000,
//!       "text": "Make it so." },
//!     …
//!   ]
//! }
//! ```
//!
//! Single `println!` reports the produced snapshot path on stdout so
//! a genrule can capture it — that one call has the lint suppressed
//! at the callsite; the rest of the binary follows the workspace's
//! "no println / eprintln" convention.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use frankweiler_signal_backup::{
    backup,
    write::{write_snapshot, SnapshotInput},
};
use serde::Deserialize;

fn main() -> Result<()> {
    let mut args = std::env::args().skip(1);
    let spec_path = args
        .next()
        .ok_or_else(|| anyhow!("usage: signal-make-fixture <spec.json> <out_dir>"))?;
    let out_root = args
        .next()
        .ok_or_else(|| anyhow!("usage: signal-make-fixture <spec.json> <out_dir>"))?;
    let out_root = PathBuf::from(out_root);

    let raw =
        std::fs::read_to_string(&spec_path).with_context(|| format!("read spec {spec_path}"))?;
    let spec: Spec = serde_json::from_str(&raw).context("parse spec json")?;
    let aep = spec.aep.unwrap_or_else(|| "0".repeat(64));
    let snapshot_dir_name = spec
        .snapshot_dir_name
        .unwrap_or_else(|| "signal-backup-2364-04-09-12-00-00".to_string());
    let out_dir = out_root.join(&snapshot_dir_name);

    let mut frames: Vec<backup::Frame> = Vec::new();
    for r in &spec.recipients {
        frames.push(recipient_frame(r));
    }
    for c in &spec.chats {
        frames.push(chat_frame(c));
    }
    for ci in &spec.chat_items {
        frames.push(chat_item_frame(ci));
    }

    let backup_info = backup::BackupInfo {
        version: 1,
        backup_time_ms: spec.backup_time_ms.unwrap_or(1_700_000_000_000),
        ..Default::default()
    };

    write_snapshot(
        &out_dir,
        &SnapshotInput {
            aep: &aep,
            // Deterministic: a "make-it-so" pun is the right choice
            // when the AEP is also a known constant.
            backup_id: b"makeitsomakeitso",
            metadata_iv: &[0u8; 12],
            main_iv: &[0u8; 16],
            backup_info,
            frames: &frames,
            file_names: &spec.file_names.unwrap_or_default(),
        },
    )?;
    #[allow(clippy::disallowed_macros)]
    {
        println!("wrote {}", out_dir.display());
    }
    Ok(())
}

#[derive(Debug, Deserialize)]
struct Spec {
    #[serde(default)]
    aep: Option<String>,
    #[serde(default)]
    snapshot_dir_name: Option<String>,
    #[serde(default)]
    backup_time_ms: Option<u64>,
    #[serde(default)]
    recipients: Vec<RecipientSpec>,
    #[serde(default)]
    chats: Vec<ChatSpec>,
    #[serde(default)]
    chat_items: Vec<ChatItemSpec>,
    #[serde(default)]
    file_names: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct RecipientSpec {
    id: u64,
    #[serde(default, rename = "self")]
    is_self: bool,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    e164: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct ChatSpec {
    id: u64,
    recipient_id: u64,
}

#[derive(Debug, Deserialize)]
struct ChatItemSpec {
    chat_id: u64,
    author_id: u64,
    date_sent: u64,
    text: String,
    #[serde(default)]
    outgoing: bool,
}

fn recipient_frame(r: &RecipientSpec) -> backup::Frame {
    use backup::recipient::Destination;
    let destination = if r.is_self {
        Some(Destination::Self_(backup::Self_::default()))
    } else {
        let (given, family) = split_name(r.name.as_deref().unwrap_or(""));
        Some(Destination::Contact(backup::Contact {
            e164: r.e164,
            profile_given_name: given,
            profile_family_name: family,
            ..Default::default()
        }))
    };
    backup::Frame {
        item: Some(backup::frame::Item::Recipient(backup::Recipient {
            id: r.id,
            destination,
        })),
    }
}

fn split_name(full: &str) -> (Option<String>, Option<String>) {
    if full.is_empty() {
        return (None, None);
    }
    let mut it = full.splitn(2, ' ');
    let given = it.next().map(|s| s.to_string());
    let family = it.next().map(|s| s.to_string()).filter(|s| !s.is_empty());
    (given, family)
}

fn chat_frame(c: &ChatSpec) -> backup::Frame {
    backup::Frame {
        item: Some(backup::frame::Item::Chat(backup::Chat {
            id: c.id,
            recipient_id: c.recipient_id,
            ..Default::default()
        })),
    }
}

fn chat_item_frame(ci: &ChatItemSpec) -> backup::Frame {
    use backup::chat_item;
    let item = chat_item::Item::StandardMessage(backup::StandardMessage {
        text: Some(backup::Text {
            body: ci.text.clone(),
            body_ranges: vec![],
        }),
        ..Default::default()
    });
    let directional = if ci.outgoing {
        Some(chat_item::DirectionalDetails::Outgoing(
            backup::chat_item::OutgoingMessageDetails::default(),
        ))
    } else {
        Some(chat_item::DirectionalDetails::Incoming(
            backup::chat_item::IncomingMessageDetails::default(),
        ))
    };
    backup::Frame {
        item: Some(backup::frame::Item::ChatItem(backup::ChatItem {
            chat_id: ci.chat_id,
            author_id: ci.author_id,
            date_sent: ci.date_sent,
            directional_details: directional,
            item: Some(item),
            ..Default::default()
        })),
    }
}
