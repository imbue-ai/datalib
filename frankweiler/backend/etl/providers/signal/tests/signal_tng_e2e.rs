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
    backup,
    write::{write_snapshot, SnapshotInput},
};

const FIXTURE_AEP: &str = "0000000000000000000000000000000000000000000000000000000000000000";

#[tokio::test(flavor = "multi_thread")]
async fn extract_then_translate_against_tng_fixture() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let snapshot_root = tmp.path().join("snapshots");
    let data_root = tmp.path().join("data");
    std::fs::create_dir_all(&snapshot_root)?;
    std::fs::create_dir_all(&data_root)?;

    write_tng_snapshot(&snapshot_root)?;

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
        aep_env_var: None, // defaults to SIGNAL_PASSPHRASE
        progress: Progress::noop(),
        control: ExtractControl::default(),
    })
    .await?;
    assert_eq!(summary.recipients, 3, "expected 3 recipients");
    assert_eq!(summary.chats, 1, "expected 1 chat");
    assert_eq!(summary.chat_items, 4, "expected 4 chat items");

    // Translate runs against the doltlite-extended sqlite the
    // extractor wrote. parse_raw_dir wants the raw path (without the
    // .doltlite_db extension) — extract::fetch normalized it the
    // same way internally.
    let parsed = tokio::task::spawn_blocking({
        let raw = raw_db_path.clone();
        move || parse_raw_dir(&raw)
    })
    .await??;
    assert_eq!(parsed.chats.len(), 1, "expected 1 chat parsed");
    assert_eq!(parsed.recipients.len(), 3, "expected 3 recipients parsed");

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
        assert_eq!(render_summary.chats_rendered, 1);
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
    // 1 chat-level row + 4 message-level rows = 5 grid rows.
    assert_eq!(doc.rows.len(), 5, "1 chat row + 4 message rows");

    let sidecar_path = doc.md_path.with_extension("grid_rows.json");
    assert!(sidecar_path.exists(), "sidecar written next to md");

    Ok(())
}

/// Write a small TNG snapshot inline (smaller than the checked-in
/// JSON fixture) — keeps the test self-contained and lets us assert
/// exact counts without re-reading the JSON.
fn write_tng_snapshot(root: &Path) -> Result<()> {
    let frames = vec![
        recipient_self(1, "Jean-Luc Picard"),
        recipient_contact(2, "Will Riker", 17015550101),
        recipient_contact(3, "Data Soong", 17015550102),
        chat_frame(100, 2),
        chat_item(100, 1, 12442118400000, "Status report.", true),
        chat_item(100, 2, 12442118460000, "All decks at green status.", false),
        chat_item(100, 3, 12442118520000, "Sensors detect a vessel.", false),
        chat_item(100, 1, 12442118940000, "Make it so.", true),
    ];
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
            file_names: &[],
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
