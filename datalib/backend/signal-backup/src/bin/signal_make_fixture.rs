//! `signal-make-fixture <spec.json> <out_dir>` — produces an
//! encrypted Signal-Android backup folder from a TNG-themed JSON spec:
//! one snapshot directory, the shared `files/` tree its attachments are
//! encrypted into, and, when the spec has a `new_phone`, a second
//! snapshot as a phone restored from the first would write it. AEP
//! defaults to `"0" * 64` (the fixture AEP) so the extractor sees a
//! real backup it can decrypt with a known-public passphrase.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};
use datalib_signal_backup::{
    backup, encrypt_attachment, local_media_name,
    write::{write_snapshot, SnapshotInput},
};
use serde::Deserialize;
use sha2::{Digest, Sha256};

/// A 1×1 transparent PNG: every attachment's bytes.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

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
    let aep = spec.aep.clone().unwrap_or_else(|| "0".repeat(64));
    let snapshot_dir_name = spec
        .snapshot_dir_name
        .clone()
        .unwrap_or_else(|| "signal-backup-2364-04-09-12-00-00".to_string());

    // The old phone holds every attachment's bytes, encrypted into the
    // shared tree; the new phone has downloaded none of them.
    let mut stored: HashMap<(usize, usize), Stored> = HashMap::new();
    for (i, ci) in spec.chat_items.iter().enumerate() {
        for slot in 0..ci.attachments.len() {
            let n = stored.len();
            stored.insert((i, slot), store_attachment(&out_root.join("files"), n)?);
        }
    }
    let mut file_names: Vec<String> = spec.file_names.clone().unwrap_or_default();
    file_names.extend(stored.values().map(|s| s.media_name.clone()));
    file_names.sort();

    let same = Numbering::same(&spec);
    write(
        &out_root.join(&snapshot_dir_name),
        &aep,
        spec.backup_time_ms.unwrap_or(1_700_000_000_000),
        &frames(&spec, &spec.chat_items, &same, Some(&stored)),
        &file_names,
    )?;

    if let Some(new) = &spec.new_phone {
        let items: Vec<ChatItemSpec> = spec
            .chat_items
            .iter()
            .chain(&new.chat_items)
            .cloned()
            .collect();
        write(
            &out_root.join(&new.snapshot_dir_name),
            &aep,
            new.backup_time_ms,
            &frames(&spec, &items, &Numbering::anew(&spec), None),
            &[],
        )?;
    }
    Ok(())
}

fn write(
    out_dir: &Path,
    aep: &str,
    backup_time_ms: u64,
    frames: &[backup::Frame],
    file_names: &[String],
) -> Result<()> {
    write_snapshot(
        out_dir,
        &SnapshotInput {
            aep,
            // Deterministic: a "make-it-so" pun is the right choice
            // when the AEP is also a known constant.
            backup_id: b"makeitsomakeitso",
            metadata_iv: &[0u8; 12],
            main_iv: &[0u8; 16],
            backup_info: backup::BackupInfo {
                version: 1,
                backup_time_ms,
                ..Default::default()
            },
            frames,
            file_names,
        },
    )?;
    #[allow(clippy::disallowed_macros)]
    {
        println!("wrote {}", out_dir.display());
    }
    Ok(())
}

/// The numbers a backup gives the spec's recipients and chats, in the
/// order it lists them. The spec writes everything in the first
/// backup's numbers; a phone restored from it lists everyone in another
/// order and numbers them from 1, as a fresh database does.
struct Numbering {
    recipients: Vec<(u64, u64)>,
    chats: Vec<(u64, u64)>,
}

impl Numbering {
    fn same(spec: &Spec) -> Self {
        Self {
            recipients: spec.recipients.iter().map(|r| (r.id, r.id)).collect(),
            chats: spec.chats.iter().map(|c| (c.id, c.id)).collect(),
        }
    }

    fn anew(spec: &Spec) -> Self {
        let renumber = |ids: Vec<u64>| ids.into_iter().rev().zip(1..).collect();
        Self {
            recipients: renumber(spec.recipients.iter().map(|r| r.id).collect()),
            chats: renumber(spec.chats.iter().map(|c| c.id).collect()),
        }
    }

    fn recipient(&self, id: u64) -> u64 {
        lookup(&self.recipients, id)
    }

    fn chat(&self, id: u64) -> u64 {
        lookup(&self.chats, id)
    }
}

fn lookup(pairs: &[(u64, u64)], id: u64) -> u64 {
    pairs
        .iter()
        .find(|(from, _)| *from == id)
        .map(|(_, to)| *to)
        .unwrap_or_else(|| panic!("the spec names {id}, which it does not list"))
}

/// Every frame of one backup: recipients, then chats, then items, each
/// a backup lists before anything names it.
fn frames(
    spec: &Spec,
    items: &[ChatItemSpec],
    numbering: &Numbering,
    stored: Option<&HashMap<(usize, usize), Stored>>,
) -> Vec<backup::Frame> {
    let mut frames: Vec<backup::Frame> = Vec::new();
    for (from, to) in &numbering.recipients {
        let r = spec.recipients.iter().find(|r| r.id == *from).unwrap();
        frames.push(recipient_frame(r, *to));
    }
    for (from, to) in &numbering.chats {
        let c = spec.chats.iter().find(|c| c.id == *from).unwrap();
        frames.push(chat_frame(*to, numbering.recipient(c.recipient_id)));
    }
    for (i, ci) in items.iter().enumerate() {
        let attachments = (0..ci.attachments.len())
            .map(|slot| {
                attachment(
                    &ci.attachments[slot],
                    stored.and_then(|s| s.get(&(i, slot))),
                )
            })
            .collect();
        frames.push(chat_item_frame(ci, numbering, attachments));
    }
    frames
}

/// One attachment's bytes as the old phone stored them.
struct Stored {
    local_key: [u8; 64],
    plaintext_hash: Vec<u8>,
    media_name: String,
}

/// Encrypt the `n`th attachment into `<files_root>/<XX>/<media_name>`,
/// the layout Signal Android writes, under a key of its own.
fn store_attachment(files_root: &Path, n: usize) -> Result<Stored> {
    let mut local_key = [0u8; 64];
    for (i, b) in local_key.iter_mut().enumerate() {
        *b = (i as u8).wrapping_mul(31) ^ (n as u8);
    }
    let plaintext_hash = Sha256::digest(TINY_PNG).to_vec();
    let media_name = local_media_name(&plaintext_hash, &local_key);
    let dir = files_root.join(&media_name[..2]);
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    std::fs::write(
        dir.join(&media_name),
        encrypt_attachment(TINY_PNG, &local_key, &[0u8; 16]),
    )?;
    Ok(Stored {
        local_key,
        plaintext_hash,
        media_name,
    })
}

/// An attachment slot: with what finds and decrypts its bytes where the
/// backup holds them, and otherwise as Signal writes one the phone never
/// downloaded.
fn attachment(a: &AttachmentSpec, stored: Option<&Stored>) -> backup::MessageAttachment {
    use backup::file_pointer::locator_info::IntegrityCheck;
    let locator = backup::file_pointer::LocatorInfo {
        key: vec![0x5a; 64],
        size: TINY_PNG.len() as u32,
        local_key: stored.map(|s| s.local_key.to_vec()),
        integrity_check: stored.map(|s| IntegrityCheck::PlaintextHash(s.plaintext_hash.clone())),
        ..Default::default()
    };
    backup::MessageAttachment {
        pointer: Some(backup::FilePointer {
            content_type: Some(a.content_type.clone()),
            file_name: Some(a.file_name.clone()),
            width: Some(1),
            height: Some(1),
            locator_info: Some(locator),
            ..Default::default()
        }),
        was_downloaded: stored.is_some(),
        ..Default::default()
    }
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
    /// A second snapshot, as the phone the account moved to writes it.
    #[serde(default)]
    new_phone: Option<NewPhoneSpec>,
}

/// The phone the account moved to: everyone renumbered, no attachment
/// downloaded, and the messages since the switch. Its items are written
/// in the first backup's numbers.
#[derive(Debug, Deserialize)]
struct NewPhoneSpec {
    snapshot_dir_name: String,
    backup_time_ms: u64,
    #[serde(default)]
    chat_items: Vec<ChatItemSpec>,
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
    /// A UUID, dashed or not: the account's id.
    #[serde(default)]
    aci: Option<String>,
    /// A UUID: the id behind the number alone, for a contact the
    /// account has not messaged.
    #[serde(default)]
    pni: Option<String>,
    /// A group, in place of a contact.
    #[serde(default)]
    group: Option<GroupSpec>,
}

#[derive(Debug, Deserialize)]
struct GroupSpec {
    title: String,
    /// 32 bytes, as hex.
    master_key: String,
}

#[derive(Debug, Clone, Deserialize)]
struct AttachmentSpec {
    file_name: String,
    content_type: String,
}

fn uuid_bytes(s: &str) -> Vec<u8> {
    let bytes = hex_bytes(&s.replace('-', ""));
    assert_eq!(bytes.len(), 16, "{s:?} is not a UUID");
    bytes
}

fn hex_bytes(hex: &str) -> Vec<u8> {
    assert!(hex.len().is_multiple_of(2), "{hex:?} is not hex");
    (0..hex.len() / 2)
        .map(|i| u8::from_str_radix(&hex[2 * i..2 * i + 2], 16).expect("hex"))
        .collect()
}

#[derive(Debug, Deserialize)]
struct ChatSpec {
    id: u64,
    recipient_id: u64,
}

#[derive(Debug, Clone, Deserialize)]
struct ChatItemSpec {
    chat_id: u64,
    author_id: u64,
    date_sent: u64,
    text: String,
    #[serde(default)]
    outgoing: bool,
    /// An incoming message the account has not read; read otherwise.
    #[serde(default)]
    unread: bool,
    /// The ACI each `U+FFFC` in `text` mentions, in order, as Signal
    /// marks a mention up.
    #[serde(default)]
    mentions: Vec<String>,
    #[serde(default)]
    attachments: Vec<AttachmentSpec>,
}

fn recipient_frame(r: &RecipientSpec, id: u64) -> backup::Frame {
    use backup::recipient::Destination;
    let destination = if r.is_self {
        Some(Destination::Self_(backup::Self_::default()))
    } else if let Some(g) = &r.group {
        use backup::group::{group_attribute_blob::Content, GroupAttributeBlob, GroupSnapshot};
        Some(Destination::Group(backup::Group {
            master_key: hex_bytes(&g.master_key),
            snapshot: Some(GroupSnapshot {
                title: Some(GroupAttributeBlob {
                    content: Some(Content::Title(g.title.clone())),
                }),
                ..Default::default()
            }),
            ..Default::default()
        }))
    } else {
        let (given, family) = split_name(r.name.as_deref().unwrap_or(""));
        Some(Destination::Contact(backup::Contact {
            e164: r.e164,
            aci: r.aci.as_deref().map(uuid_bytes),
            pni: r.pni.as_deref().map(uuid_bytes),
            profile_given_name: given,
            profile_family_name: family,
            ..Default::default()
        }))
    };
    backup::Frame {
        item: Some(backup::frame::Item::Recipient(backup::Recipient {
            id,
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

fn chat_frame(id: u64, recipient_id: u64) -> backup::Frame {
    backup::Frame {
        item: Some(backup::frame::Item::Chat(backup::Chat {
            id,
            recipient_id,
            ..Default::default()
        })),
    }
}

/// One `mentionAci` range per placeholder, at its offset in UTF-16
/// units, which is how Signal counts.
fn mention_ranges(text: &str, acis: &[String]) -> Vec<backup::BodyRange> {
    let placeholders: Vec<u32> = text
        .chars()
        .scan(0u32, |at, c| {
            let here = *at;
            *at += c.len_utf16() as u32;
            Some((here, c))
        })
        .filter(|(_, c)| *c == '\u{FFFC}')
        .map(|(at, _)| at)
        .collect();
    assert_eq!(
        placeholders.len(),
        acis.len(),
        "{text:?}: one mention per placeholder"
    );
    placeholders
        .into_iter()
        .zip(acis)
        .map(|(start, aci)| backup::BodyRange {
            start,
            length: 1,
            associated_value: Some(backup::body_range::AssociatedValue::MentionAci(uuid_bytes(
                aci,
            ))),
        })
        .collect()
}

fn chat_item_frame(
    ci: &ChatItemSpec,
    numbering: &Numbering,
    attachments: Vec<backup::MessageAttachment>,
) -> backup::Frame {
    use backup::chat_item;
    let item = chat_item::Item::StandardMessage(backup::StandardMessage {
        text: Some(backup::Text {
            body: ci.text.clone(),
            body_ranges: mention_ranges(&ci.text, &ci.mentions),
        }),
        attachments,
        ..Default::default()
    });
    let directional = if ci.outgoing {
        Some(chat_item::DirectionalDetails::Outgoing(
            backup::chat_item::OutgoingMessageDetails::default(),
        ))
    } else {
        Some(chat_item::DirectionalDetails::Incoming(
            backup::chat_item::IncomingMessageDetails {
                read: !ci.unread,
                ..Default::default()
            },
        ))
    };
    backup::Frame {
        item: Some(backup::frame::Item::ChatItem(backup::ChatItem {
            chat_id: numbering.chat(ci.chat_id),
            author_id: numbering.recipient(ci.author_id),
            date_sent: ci.date_sent,
            directional_details: directional,
            item: Some(item),
            ..Default::default()
        })),
    }
}
