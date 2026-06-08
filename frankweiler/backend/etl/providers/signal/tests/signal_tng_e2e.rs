//! End-to-end test: write an encrypted TNG snapshot via the
//! [`frankweiler_signal_backup::write`] writer, drive
//! [`frankweiler_etl_signal::extract::fetch`] over it, then drive the
//! translate path, and assert on both the doltlite row counts and the
//! rendered markdown.
//!
//! Uses the published fixture AEP (64 zeros) — the same one the
//! checked-in `tests/fixtures/signal_tng/tng.json` spec carries — so
//! the crypto path runs exactly as it would against a real backup;
//! we just publish the key.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use frankweiler_etl::control::ExtractControl;
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl_signal::extract::{self, FetchOptions};
use frankweiler_etl_signal::translate::{parse_raw_dir, render_all};
use frankweiler_signal_backup::{
    backup, encrypt_attachment, local_media_name,
    write::{write_snapshot, SnapshotInput},
};
use sha2::{Digest, Sha256};

const FIXTURE_AEP: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// 67-byte minimal valid PNG: 1×1 transparent. Hand-assembled rather
/// than read from disk so the fixture stays self-contained — any
/// image viewer can render the file the test writes into the
/// rendered_md tree.
const TINY_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, // PNG signature
    0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52, // IHDR length + type
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, // width=1, height=1
    0x08, 0x06, 0x00, 0x00, 0x00, // bit depth=8, color=6 (RGBA)
    0x1f, 0x15, 0xc4, 0x89, // IHDR CRC
    0x00, 0x00, 0x00, 0x0d, 0x49, 0x44, 0x41, 0x54, // IDAT length + type
    0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d,
    0xb4, // IDAT data + CRC
    0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, // IEND length + type
    0xae, 0x42, 0x60, 0x82, // IEND CRC
];

/// 64-byte local key for the test attachment. Real backups generate
/// these per-attachment from CSPRNG; for fixture determinism we use a
/// fixed value (and document it here so anyone reading the test
/// knows it isn't a secret).
const TEST_LOCAL_KEY: [u8; 64] = [0xab; 64];

/// Encrypt + write the TINY_PNG bytes into the `<files_root>/<XX>/<name>`
/// layout extract walks. Returns the `media_name` for the caller to
/// drop into a `FilePointer.LocatorInfo`.
fn write_test_attachment(files_root: &Path) -> Result<(String, Vec<u8>)> {
    let plaintext = TINY_PNG.to_vec();
    let plaintext_hash = Sha256::digest(&plaintext).to_vec();
    let media_name = local_media_name(&plaintext_hash, &TEST_LOCAL_KEY);
    // Determinism: fixed IV like the snapshot writer uses elsewhere.
    let enc = encrypt_attachment(&plaintext, &TEST_LOCAL_KEY, &[0u8; 16]);
    let shard = &media_name[..2];
    let dir = files_root.join(shard);
    std::fs::create_dir_all(&dir)?;
    std::fs::write(dir.join(&media_name), &enc)?;
    Ok((media_name, plaintext_hash))
}

/// Build a `MessageAttachment` (the `repeated MessageAttachment
/// attachments` slot on `StandardMessage`) whose `LocatorInfo`
/// points at the encrypted bytes we wrote into `files/`.
fn png_attachment(plaintext_hash: &[u8]) -> backup::MessageAttachment {
    use backup::file_pointer::locator_info::IntegrityCheck;
    backup::MessageAttachment {
        pointer: Some(backup::FilePointer {
            content_type: Some("image/png".into()),
            file_name: Some("delta-shield.png".into()),
            width: Some(1),
            height: Some(1),
            locator_info: Some(backup::file_pointer::LocatorInfo {
                key: TEST_LOCAL_KEY[..32].to_vec(),
                size: TINY_PNG.len() as u32,
                local_key: Some(TEST_LOCAL_KEY.to_vec()),
                integrity_check: Some(IntegrityCheck::PlaintextHash(plaintext_hash.to_vec())),
                ..Default::default()
            }),
            ..Default::default()
        }),
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn extract_then_translate_against_tng_fixture() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let snapshot_root = tmp.path().join("snapshots");
    let data_root = tmp.path().join("data");
    std::fs::create_dir_all(&snapshot_root)?;
    std::fs::create_dir_all(&data_root)?;

    // Stash one encrypted PNG under `<snapshot_root>/files/<XX>/<name>`
    // — the layout extract walks. Picard's "Make it so." message will
    // reference this attachment so the rendered .md should contain an
    // `![…](blobs/…png)` image link.
    let files_root = snapshot_root.join("files");
    let (png_media_name, png_plaintext_hash) = write_test_attachment(&files_root)?;

    write_tng_snapshot(&snapshot_root, &png_media_name, &png_plaintext_hash)?;

    let raw_db_path = data_root.join("raw").join("signal");
    std::fs::create_dir_all(raw_db_path.parent().unwrap())?;

    // SAFETY: bazel test runs each test target in a fresh process, so
    // mutating the env here is hermetic — no other test sees it.
    unsafe {
        std::env::set_var("SIGNAL_PASSPHRASE", FIXTURE_AEP);
    }

    let summary = extract::fetch(FetchOptions {
        db_path: raw_db_path.clone(),
        db: None,
        snapshot_root: snapshot_root.clone(),
        files_root: None, // defaults to snapshot_root/files (the layout the fixture writes)
        aep_env_var: None, // defaults to SIGNAL_PASSPHRASE
        progress: Progress::noop(),
        control: ExtractControl::default(),
    })
    .await?;
    assert_eq!(summary.recipients, 3, "expected 3 recipients");
    assert_eq!(summary.chats, 1, "expected 1 chat");
    assert_eq!(summary.chat_items, 4, "expected 4 chat items");
    assert_eq!(
        summary.blobs, 1,
        "the 'Make it so.' message carries one attached PNG"
    );
    assert_eq!(summary.blob_errors, 0, "no extract-side blob errors");

    // Translate runs against the doltlite-extended sqlite the
    // extractor wrote. parse_raw_dir wants the raw path (without the
    // .doltlite_db extension) — extract::fetch normalized it the
    // same way internally. Default period (Month) is fine here; all
    // 4 messages share a single month (2364-04) so one bucket.
    let parsed = tokio::task::spawn_blocking({
        let raw = raw_db_path.clone();
        move || parse_raw_dir(&raw)
    })
    .await??;
    assert_eq!(parsed.chats.len(), 1, "expected 1 chat parsed");
    assert_eq!(parsed.recipients.len(), 3, "expected 3 recipients parsed");
    assert_eq!(
        parsed.docs.len(),
        1,
        "expected 1 (chat, period_key) bucket; all messages in same month"
    );
    assert_eq!(parsed.docs[0].period_key, "2364-04");

    let progress = Progress::noop();
    let prior: HashMap<String, String> = HashMap::new();
    let mut rendered_docs: Vec<RenderedMarkdown> = Vec::new();
    {
        let mut on_doc_complete = |doc: RenderedMarkdown| -> Result<()> {
            rendered_docs.push(doc);
            Ok(())
        };
        let render_summary = render_all(
            &parsed,
            &data_root,
            "signal-tng",
            &progress,
            &prior,
            &mut on_doc_complete,
        )?;
        assert_eq!(render_summary.docs_rendered, 1);
        assert_eq!(render_summary.messages_rendered, 4);
    }

    assert_eq!(rendered_docs.len(), 1, "expected one rendered doc");
    let doc = &rendered_docs[0];
    let md = std::fs::read_to_string(&doc.md_path)?;
    assert!(
        md.contains("# Signal · Will Riker"),
        "title heading present"
    );
    assert!(md.contains("Make it so."), "Picard's order rendered");
    assert!(md.contains("_Me_:"), "outgoing author labelled as Me");
    assert!(
        md.contains("_Will Riker_:"),
        "Riker's name resolved from recipient"
    );

    // PNG attachment surfaces as an inline image link under the
    // "Make it so." bullet, with the upstream filename as the alt
    // text and the hash-based filename as the target.
    assert!(
        md.contains("![delta-shield.png](blobs/"),
        "expected the image attachment to render as an inline link in:\n{md}"
    );
    let png_dir = doc
        .md_path
        .parent()
        .expect("md has a parent dir")
        .join("blobs");
    let written_pngs: Vec<_> = std::fs::read_dir(&png_dir)?
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().and_then(|s| s.to_str()) == Some("png"))
        .collect();
    assert_eq!(
        written_pngs.len(),
        1,
        "exactly one PNG materialized on disk"
    );
    let bytes = std::fs::read(written_pngs[0].path())?;
    assert_eq!(
        bytes, TINY_PNG,
        "materialized PNG matches the plaintext we encrypted in"
    );

    // 1 chat-level row + 4 message-level rows = 5 grid rows.
    assert_eq!(doc.rows.len(), 5, "1 chat row + 4 message rows");

    let sidecar_path = doc.md_path.with_extension("grid_rows.json");
    assert!(sidecar_path.exists(), "sidecar written next to md");

    Ok(())
}

/// Write a small TNG snapshot inline (smaller than the checked-in
/// JSON fixture) — keeps the test self-contained and lets us assert
/// exact counts without re-reading the JSON. The PNG attachment hangs
/// off Picard's "Make it so." message; the rest are plain text.
fn write_tng_snapshot(root: &Path, png_media_name: &str, png_plaintext_hash: &[u8]) -> Result<()> {
    let make_it_so = {
        let mut frame = chat_item(100, 1, 12442118940000, "Make it so.", true);
        if let Some(backup::frame::Item::ChatItem(ci)) = frame.item.as_mut() {
            if let Some(backup::chat_item::Item::StandardMessage(sm)) = ci.item.as_mut() {
                sm.attachments.push(png_attachment(png_plaintext_hash));
            }
        }
        frame
    };
    let frames = vec![
        recipient_self(1, "Jean-Luc Picard"),
        recipient_contact(2, "Will Riker", 17015550101),
        recipient_contact(3, "Data Soong", 17015550102),
        chat_frame(100, 2),
        chat_item(100, 1, 12442118400000, "Status report.", true),
        chat_item(100, 2, 12442118460000, "All decks at green status.", false),
        chat_item(100, 3, 12442118520000, "Sensors detect a vessel.", false),
        make_it_so,
    ];
    let file_names = vec![png_media_name.to_string()];
    write_snapshot(
        &root.join("signal-backup-2364-04-09-12-00-00"),
        &SnapshotInput {
            aep: FIXTURE_AEP,
            backup_id: b"makeitsomakeitso",
            metadata_iv: &[0u8; 12],
            main_iv: &[0u8; 16],
            backup_info: backup::BackupInfo {
                version: 1,
                backup_time_ms: 12442118400000,
                ..Default::default()
            },
            frames: &frames,
            file_names: &file_names,
        },
    )?;
    Ok(())
}

fn recipient_self(id: u64, _name: &str) -> backup::Frame {
    use backup::recipient::Destination;
    backup::Frame {
        item: Some(backup::frame::Item::Recipient(backup::Recipient {
            id,
            destination: Some(Destination::Self_(backup::Self_::default())),
        })),
    }
}

fn recipient_contact(id: u64, full: &str, e164: u64) -> backup::Frame {
    use backup::recipient::Destination;
    let mut sp = full.splitn(2, ' ');
    let given = sp.next().map(|s| s.to_string());
    let family = sp.next().map(|s| s.to_string());
    backup::Frame {
        item: Some(backup::frame::Item::Recipient(backup::Recipient {
            id,
            destination: Some(Destination::Contact(backup::Contact {
                e164: Some(e164),
                profile_given_name: given,
                profile_family_name: family,
                ..Default::default()
            })),
        })),
    }
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

fn chat_item(
    chat_id: u64,
    author_id: u64,
    date_sent: u64,
    text: &str,
    outgoing: bool,
) -> backup::Frame {
    use backup::chat_item;
    let directional = if outgoing {
        chat_item::DirectionalDetails::Outgoing(backup::chat_item::OutgoingMessageDetails::default())
    } else {
        chat_item::DirectionalDetails::Incoming(backup::chat_item::IncomingMessageDetails::default())
    };
    backup::Frame {
        item: Some(backup::frame::Item::ChatItem(backup::ChatItem {
            chat_id,
            author_id,
            date_sent,
            directional_details: Some(directional),
            item: Some(chat_item::Item::StandardMessage(backup::StandardMessage {
                text: Some(backup::Text {
                    body: text.to_string(),
                    body_ranges: vec![],
                }),
                ..Default::default()
            })),
            ..Default::default()
        })),
    }
}
