//! End-to-end test: write an encrypted TNG snapshot via the
//! [`datalib_signal_backup::write`] writer, drive
//! [`datalib_etl_signal::ingest::fetch`] over it, then drive the
//! render path, and assert on both the doltlite row counts and the
//! rendered markdown.

use std::path::Path;

use anyhow::Result;
use datalib_etl::control::DownloadControl;
use datalib_etl::progress::Progress;
use datalib_etl_files::fingerprint_cache::FingerprintCache;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_signal::ingest::{self, FetchOptions};
use datalib_etl_signal_render::render::{parse_raw_dir, render_all};
use datalib_signal_backup::{
    backup, encrypt_attachment, local_media_name,
    write::{write_snapshot, SnapshotInput},
};
use sha2::{Digest, Sha256};

const FIXTURE_AEP: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// 67-byte minimal valid PNG: 1×1 transparent. Hand-assembled rather
/// than read from disk so the fixture stays self-contained — any
/// image viewer can render the file the test writes into the
/// render_markdown tree.
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
/// layout download walks. Returns the `media_name` for the caller to
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
    // — the layout download walks. Picard's "Make it so." message will
    // reference this attachment so the rendered .md should contain an
    // `![…](/data/…/blob/<hash>)` image link.
    let files_root = snapshot_root.join("files");
    let (png_media_name, png_plaintext_hash) = write_test_attachment(&files_root)?;

    write_tng_snapshot(&snapshot_root, &png_media_name, &png_plaintext_hash)?;

    let raw_db_path = data_root.join("raw").join("signal");
    std::fs::create_dir_all(raw_db_path.parent().unwrap())?;

    // A temp cache: tests must never touch this host's real one.
    let cache = FingerprintCache::open(&tmp.path().join("fingerprints.sqlite")).await?;
    // One handle for the whole pass: the file takes one writer at
    // a time.
    let db = ingest::RawDb::open(&datalib_etl::doltlite_raw::db_path_for(&raw_db_path)).await?;
    let summary = ingest::fetch(FetchOptions {
        db: db.clone(),
        cache,
        snapshot_root: snapshot_root.clone(),
        files_root: None, // defaults to snapshot_root/files (the layout the fixture writes)
        aep: FIXTURE_AEP.into(),
        progress: Progress::noop(),
        control: DownloadControl::default(),
    })
    .await?;
    assert_eq!(summary.recipients, 3, "expected 3 recipients");
    assert_eq!(summary.chats, 1, "expected 1 chat");
    assert_eq!(summary.chat_items, 4, "expected 4 chat items");
    assert_eq!(
        summary.blobs, 1,
        "the 'Make it so.' message carries one attached PNG"
    );
    assert_eq!(summary.blob_errors, 0, "no download-side blob errors");

    // Mirror what the orchestrator does in production: dolt_commit
    // the freshly-extracted rows so the `dolt_diff_<table>` vtabs the
    // render path queries actually resolve. Without a commit the
    // vtabs may report "no such table" on a brand-new working set,
    // and the second-pass docs_skipped assertion below would fail.
    // Self-skips on stock libsqlite3.
    {
        for q in [
            "SELECT dolt_config('user.name', 'datalib-test')",
            "SELECT dolt_config('user.email', 'test@datalib.local')",
        ] {
            let _ = sqlx::query(q).execute(db.pool()).await;
        }
        let _ = datalib_etl::store_handle::RawStoreHandle::commit_all(&db, "test download").await;
    }
    db.close().await;

    // Render runs against the doltlite-extended sqlite the
    // extractor wrote. parse_raw_dir wants the raw path (without the
    // .doltlite_db extension) — ingest::fetch normalized it the
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
    assert_eq!(
        parsed.docs_skipped, 0,
        "first parse: no render cursor → cold start → nothing skipped"
    );
    assert_eq!(parsed.docs[0].period_key, "2364-04");

    let progress = Progress::noop();
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
            &mut on_doc_complete,
        )?;
        assert_eq!(render_summary.docs_rendered, 1);
        assert_eq!(render_summary.messages_rendered, 4);
        assert_eq!(
            render_summary.docs_skipped, 0,
            "first pass: nothing to skip (no prior render cursor)"
        );
        assert_eq!(
            render_summary.docs_total, 1,
            "first pass: docs_total = docs_rendered + docs_skipped"
        );
    }

    assert_eq!(rendered_docs.len(), 1, "expected one rendered doc");
    let doc = &rendered_docs[0];
    let md = std::fs::read_to_string(&doc.md_path)?;
    // Signal uses the shared `Title` helper, which renders an
    // HTML-in-markdown `<h1 class="page-title">` block (not a
    // markdown `# …` line) so the Vue side can hook a copy-id button
    // off `data-page-title-uuid`.
    assert!(
        md.contains("class=\"page-title\"") && md.contains("Signal · Will Riker"),
        "page-title H1 with markdown_uuid hook present in:\n{md}"
    );
    assert!(md.contains("Make it so."), "Picard's order rendered");
    // Signal renders through `chat-common` now, so a message is the
    // same `## <span class="msg-author">…` header every chat source
    // writes rather than the bulleted line this provider used to emit.
    assert!(
        md.contains("<span class=\"msg-author\">Me</span>"),
        "outgoing author labelled as Me in:\n{md}"
    );
    assert!(
        md.contains("## [Will Riker](tel:+17015550101 \"Will Riker (+17015550101)\") "),
        "Riker's name resolved from recipient, his number as the handle, in:\n{md}"
    );

    // PNG attachment surfaces as an inline image under the "Make it
    // so." message, with the upstream filename as the alt text, linked
    // at its address in the source's blob store. Nothing is copied
    // beside the page.
    let png_hash = datalib_etl::blob_cas::blake3_hex(TINY_PNG);
    assert!(
        md.contains(&format!(
            "![delta-shield.png](/data/raw/signal/blob/{png_hash})"
        )),
        "expected the image attachment to link the blob store in:\n{md}"
    );
    assert!(
        !doc.md_path
            .parent()
            .expect("md has a parent dir")
            .join("blobs")
            .exists(),
        "nothing copied beside the page"
    );
    let cas = datalib_etl::blob_cas::BlobCas::open_reader(&datalib_etl::blob_cas::cas_path_for(
        &datalib_etl::doltlite_raw::db_path_for(&raw_db_path),
    ))
    .await?;
    let stored = cas.get(&png_hash).await?;
    cas.close().await;
    assert_eq!(
        stored.map(|o| o.bytes).as_deref(),
        Some(TINY_PNG),
        "the store holds the plaintext we encrypted in"
    );

    // 1 chat-level row + 4 message-level rows = 5 grid rows.
    assert_eq!(doc.rows.len(), 5, "1 chat row + 4 message rows");

    // Subject: every row — chat-level and message-level — carries the
    // bare chat name (the recipient display); a chat is filed nowhere
    // else, so `channel` stays empty.
    assert!(
        doc.rows
            .iter()
            .all(|r| r.conversation_name.as_deref() == Some("Will Riker") && r.channel.is_none()),
        "every Signal grid row's subject is the chat name, and none has a channel, got: {:?}",
        doc.rows
            .iter()
            .map(|r| (&r.conversation_name, &r.channel))
            .collect::<Vec<_>>()
    );

    // ── Second pass: prove the docs_skipped path works ─────────────
    // The commit the first pass pinned is what the render step records
    // as the cursor; the second pass diffs from it.
    let cursor = parsed
        .scan
        .new_head
        .clone()
        .expect("first render should have pinned a commit");
    assert!(
        !cursor.is_empty(),
        "cursor must carry a non-empty doltlite commit hash"
    );

    let parsed2 = tokio::task::spawn_blocking({
        let raw = raw_db_path.clone();
        let last_hash = cursor.clone();
        move || {
            // What the driver hands a provider whose declared inputs are
            // all unchanged: the cursor, and an empty stale set.
            let stale = std::collections::HashSet::new();
            datalib_etl_signal_render::render::parse(
                &raw,
                datalib_etl::periodize::Period::Month,
                "signal-tng",
                datalib_etl_render::inputs::RawRange {
                    cursor: Some(last_hash.as_str()),
                    pin: None,
                    stale: Some(&stale),
                },
            )
        }
    })
    .await??;
    assert_eq!(
        parsed2.docs.len(),
        0,
        "second parse should emit zero docs — dolt_diff says no chats changed"
    );
    assert_eq!(
        parsed2.docs_skipped, 1,
        "second parse should report one skipped chat"
    );

    let mut rendered_docs_second_pass: Vec<RenderedMarkdown> = Vec::new();
    let render_summary_2 = render_all(
        &parsed2,
        &data_root,
        "signal-tng",
        &progress,
        &mut |doc: RenderedMarkdown| -> Result<()> {
            rendered_docs_second_pass.push(doc);
            Ok(())
        },
    )?;
    assert_eq!(
        render_summary_2.docs_rendered, 0,
        "second pass should render zero docs"
    );
    assert_eq!(
        render_summary_2.docs_skipped, 1,
        "second pass should report one skipped doc"
    );
    assert_eq!(
        render_summary_2.docs_total, 1,
        "docs_total still counts the skipped chat so the \
         progress bar sums right"
    );
    assert!(
        rendered_docs_second_pass.is_empty(),
        "on_doc_complete must not fire for skipped buckets"
    );

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

async fn fetch_once(snapshot_root: &Path, store: &Path, cache: &Path) -> Result<()> {
    fetch_with(snapshot_root, store, cache, DownloadControl::default())
        .await
        .map(|_| ())
}

/// One fetch and its closing commit: HEAD after it, or `None` when the
/// run changed nothing.
async fn fetch_with(
    snapshot_root: &Path,
    store: &Path,
    cache: &Path,
    control: DownloadControl,
) -> Result<Option<String>> {
    fetch_summary(snapshot_root, store, cache, control)
        .await
        .map(|(_, moved)| moved)
}

async fn fetch_summary(
    snapshot_root: &Path,
    store: &Path,
    cache: &Path,
    control: DownloadControl,
) -> Result<(ingest::FetchSummary, Option<String>)> {
    let db = ingest::RawDb::open(&datalib_etl::doltlite_raw::db_path_for(store)).await?;
    let before = head(db.pool()).await;
    let summary = ingest::fetch(FetchOptions {
        db: db.clone(),
        cache: FingerprintCache::open(cache).await?,
        snapshot_root: snapshot_root.to_path_buf(),
        files_root: None,
        aep: FIXTURE_AEP.into(),
        progress: Progress::noop(),
        control,
    })
    .await;
    // The CAS beside it is plain SQLite and commits as it writes.
    datalib_etl::doltlite_raw::commit_run(db.pool(), "test download").await?;
    let after = head(db.pool()).await;
    db.close().await;
    summary.map(|s| (s, (after != before).then_some(after)))
}

async fn head(pool: &sqlx::SqlitePool) -> String {
    sqlx::query_scalar("SELECT commit_hash FROM dolt_log LIMIT 1")
        .fetch_one(pool)
        .await
        .unwrap_or_default()
}

async fn problems(store: &Path) -> Result<Vec<(String, String)>> {
    let db = ingest::RawDb::open(&datalib_etl::doltlite_raw::db_path_for(store)).await?;
    let rows = sqlx::query_as("SELECT scope_key, severity FROM problems ORDER BY scope_key")
        .fetch_all(db.pool())
        .await?;
    db.close().await;
    Ok(rows)
}

fn write_frames(root: &Path, name: &str, frames: &[backup::Frame]) -> Result<()> {
    write_frames_at(root, name, 12442118400000, frames)
}

fn write_frames_at(
    root: &Path,
    name: &str,
    backup_time_ms: u64,
    frames: &[backup::Frame],
) -> Result<()> {
    write_snapshot(
        &root.join(name),
        &SnapshotInput {
            aep: FIXTURE_AEP,
            backup_id: b"makeitsomakeitso",
            metadata_iv: &[0u8; 12],
            main_iv: &[0u8; 16],
            backup_info: backup::BackupInfo {
                version: 1,
                backup_time_ms,
                ..Default::default()
            },
            frames,
            file_names: &[],
        },
    )
}

/// A frame that would not decode was a log line, and the snapshot was
/// marked ingested all the same, so its records were gone with nothing
/// to say so. An attachment the backup holds no bytes for had no edge
/// and no row, though the summary's doc said it was counted.
#[tokio::test(flavor = "multi_thread")]
async fn what_a_backup_does_not_hold_is_a_problem() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let snapshots = tmp.path().join("snapshots");
    let store = tmp.path().join("raw").join("signal");
    let cache = tmp.path().join("fingerprints.sqlite");

    // Nested past prost's recursion limit, so the frame will not decode.
    let mut too_deep = backup::ChatItem {
        chat_id: 100,
        author_id: 2,
        date_sent: 12442118460000,
        ..Default::default()
    };
    for _ in 0..120 {
        too_deep = backup::ChatItem {
            revisions: vec![too_deep],
            ..Default::default()
        };
    }
    let mut not_downloaded = chat_item(100, 1, 12442118940000, "Make it so.", true);
    if let Some(backup::frame::Item::ChatItem(ci)) = not_downloaded.item.as_mut() {
        if let Some(backup::chat_item::Item::StandardMessage(sm)) = ci.item.as_mut() {
            sm.attachments.push(backup::MessageAttachment {
                pointer: Some(backup::FilePointer {
                    file_name: Some("delta-shield.png".into()),
                    ..Default::default()
                }),
                ..Default::default()
            });
        }
    }
    let crew = [
        recipient_self(1, "Jean-Luc Picard"),
        recipient_contact(2, "Will Riker", 17015550101),
        chat_frame(100, 2),
    ];
    let mut frames = crew.to_vec();
    frames.push(backup::Frame {
        item: Some(backup::frame::Item::ChatItem(too_deep)),
    });
    frames.push(not_downloaded.clone());
    write_frames(&snapshots, "signal-backup-2364-04-09-12-00-00", &frames)?;

    fetch_once(&snapshots, &store, &cache).await?;
    let item = ingest::schema_raw::chat_item_id_recipe("100", "1", 12442118940000);
    let expected = vec![
        (
            format!("chat_item_attachments:{item}#0"),
            "warning".to_string(),
        ),
        ("phase:frames".to_string(), "error".to_string()),
    ];
    assert_eq!(problems(&store).await?, expected);

    // The same snapshot is not read again, so nothing it reported clears.
    fetch_once(&snapshots, &store, &cache).await?;
    assert_eq!(problems(&store).await?, expected);

    // A newer backup is read whole: its frames all decode. It still holds
    // the message, so the message's own problem stands.
    let mut whole = crew.to_vec();
    whole.push(not_downloaded);
    write_frames(&snapshots, "signal-backup-2364-04-10-12-00-00", &whole)?;
    fetch_once(&snapshots, &store, &cache).await?;
    assert_eq!(problems(&store).await?, expected[..1].to_vec());
    Ok(())
}

/// A newer backup that no longer locates an attachment wrote its edge
/// again with no bytes and a ref of its own, so the bytes the older
/// backup decrypted were no longer reachable from the message.
#[tokio::test(flavor = "multi_thread")]
async fn a_newer_backup_without_the_bytes_keeps_the_older_copy() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let snapshots = tmp.path().join("snapshots");
    let store = tmp.path().join("raw").join("signal");
    let cache = tmp.path().join("fingerprints.sqlite");
    let (media_name, plaintext_hash) = write_test_attachment(&snapshots.join("files"))?;

    let message = |att: backup::MessageAttachment| {
        let mut frame = chat_item(100, 1, 12442118940000, "Make it so.", true);
        if let Some(backup::frame::Item::ChatItem(ci)) = frame.item.as_mut() {
            if let Some(backup::chat_item::Item::StandardMessage(sm)) = ci.item.as_mut() {
                sm.attachments.push(att);
            }
        }
        frame
    };
    let crew = [
        recipient_self(1, "Jean-Luc Picard"),
        recipient_contact(2, "Will Riker", 17015550101),
        chat_frame(100, 2),
    ];
    let mut with_bytes = crew.to_vec();
    with_bytes.push(message(png_attachment(&plaintext_hash)));
    write_frames(&snapshots, "signal-backup-2364-04-09-12-00-00", &with_bytes)?;
    fetch_once(&snapshots, &store, &cache).await?;

    let mut without = crew.to_vec();
    without.push(message(backup::MessageAttachment {
        pointer: Some(backup::FilePointer {
            file_name: Some("delta-shield.png".into()),
            ..Default::default()
        }),
        ..Default::default()
    }));
    write_frames(&snapshots, "signal-backup-2364-04-10-12-00-00", &without)?;
    fetch_once(&snapshots, &store, &cache).await?;

    let item = ingest::schema_raw::chat_item_id_recipe("100", "1", 12442118940000);
    let edge = format!("{item}#0");
    let db = ingest::RawDb::open(&datalib_etl::doltlite_raw::db_path_for(&store)).await?;
    let (ref_id, blake3): (String, Option<String>) =
        sqlx::query_as("SELECT ref_id, blake3 FROM chat_item_attachments WHERE id = ?")
            .bind(&edge)
            .fetch_one(db.pool())
            .await?;
    db.close().await;
    assert_eq!(ref_id, media_name);
    assert_eq!(
        blake3.as_deref(),
        Some(datalib_etl::blob_cas::blake3_hex(TINY_PNG).as_str())
    );
    assert_eq!(
        problems(&store).await?,
        vec![(
            format!("chat_item_attachments:{edge}"),
            "warning".to_string()
        )]
    );
    Ok(())
}

/// A stopped run still read every frame and stamped the snapshot, but
/// skipped the report, so a frame that did not decode was never said:
/// the next run takes the already-ingested shortcut.
#[tokio::test(flavor = "multi_thread")]
async fn a_stopped_run_still_reports_what_did_not_decode() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let snapshots = tmp.path().join("snapshots");
    let store = tmp.path().join("raw").join("signal");
    let cache = tmp.path().join("fingerprints.sqlite");
    let mut too_deep = backup::ChatItem {
        chat_id: 100,
        author_id: 2,
        date_sent: 12442118460000,
        ..Default::default()
    };
    for _ in 0..120 {
        too_deep = backup::ChatItem {
            revisions: vec![too_deep],
            ..Default::default()
        };
    }
    let frames = [
        recipient_self(1, "Jean-Luc Picard"),
        recipient_contact(2, "Will Riker", 17015550101),
        chat_frame(100, 2),
        backup::Frame {
            item: Some(backup::frame::Item::ChatItem(too_deep)),
        },
    ];
    write_frames(&snapshots, "signal-backup-2364-04-09-12-00-00", &frames)?;

    let control = DownloadControl::default();
    control.stop.request();
    fetch_with(&snapshots, &store, &cache, control).await?;
    fetch_once(&snapshots, &store, &cache).await?;
    assert_eq!(
        problems(&store).await?,
        vec![("phase:frames".to_string(), "error".to_string())]
    );
    Ok(())
}

async fn ids(store: &Path, table: &str) -> Result<Vec<String>> {
    let db = ingest::RawDb::open(&datalib_etl::doltlite_raw::db_path_for(store)).await?;
    let rows = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT id FROM {table} ORDER BY id"
    )))
    .fetch_all(db.pool())
    .await?;
    db.close().await;
    Ok(rows)
}

fn with_attachment(mut frame: backup::Frame, att: backup::MessageAttachment) -> backup::Frame {
    if let Some(backup::frame::Item::ChatItem(ci)) = frame.item.as_mut() {
        if let Some(backup::chat_item::Item::StandardMessage(sm)) = ci.item.as_mut() {
            sm.attachments.push(att);
        }
    }
    frame
}

/// Every snapshot was upserted over the last, so a message, a contact or
/// an attachment the newer backup no longer holds stayed in the store
/// for good.
#[tokio::test(flavor = "multi_thread")]
async fn a_newer_backup_replaces_what_the_older_one_held() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let snapshots = tmp.path().join("snapshots");
    let store = tmp.path().join("raw").join("signal");
    let cache = tmp.path().join("fingerprints.sqlite");
    let (_, plaintext_hash) = write_test_attachment(&snapshots.join("files"))?;

    let status = chat_item(100, 1, 12442118400000, "Status report.", true);
    let older = [
        recipient_self(1, "Jean-Luc Picard"),
        recipient_contact(2, "Will Riker", 17015550101),
        recipient_contact(3, "Data Soong", 17015550102),
        chat_frame(100, 2),
        status.clone(),
        chat_item(100, 2, 12442118460000, "All decks at green status.", false),
        with_attachment(
            chat_item(100, 1, 12442118940000, "Make it so.", true),
            png_attachment(&plaintext_hash),
        ),
    ];
    write_frames(&snapshots, "signal-backup-2364-04-09-12-00-00", &older)?;
    fetch_once(&snapshots, &store, &cache).await?;
    assert_eq!(ids(&store, "chat_items").await?.len(), 3);
    assert_eq!(ids(&store, "chat_item_attachments").await?.len(), 1);

    let newer = [
        recipient_self(1, "Jean-Luc Picard"),
        recipient_contact(2, "Will Riker", 17015550101),
        chat_frame(100, 2),
        status,
    ];
    write_frames(&snapshots, "signal-backup-2364-04-10-12-00-00", &newer)?;
    fetch_once(&snapshots, &store, &cache).await?;
    assert_eq!(
        ids(&store, "chat_items").await?,
        [ingest::schema_raw::chat_item_id_recipe(
            "100",
            "1",
            12442118400000
        )],
        "the messages the newer backup dropped are gone"
    );
    assert_eq!(ids(&store, "recipients").await?, ["1", "2"]);
    assert_eq!(ids(&store, "chats").await?, ["100"]);
    assert!(ids(&store, "chat_item_attachments").await?.is_empty());
    Ok(())
}

/// A backup with a frame that would not decode was not read whole, so
/// what it seems to have dropped may only be in that frame.
#[tokio::test(flavor = "multi_thread")]
async fn a_backup_not_read_whole_deletes_nothing() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let snapshots = tmp.path().join("snapshots");
    let store = tmp.path().join("raw").join("signal");
    let cache = tmp.path().join("fingerprints.sqlite");
    let crew = [
        recipient_self(1, "Jean-Luc Picard"),
        recipient_contact(2, "Will Riker", 17015550101),
        chat_frame(100, 2),
    ];
    let mut older = crew.to_vec();
    older.push(chat_item(100, 1, 12442118400000, "Status report.", true));
    older.push(chat_item(
        100,
        2,
        12442118460000,
        "All decks at green status.",
        false,
    ));
    write_frames(&snapshots, "signal-backup-2364-04-09-12-00-00", &older)?;
    fetch_once(&snapshots, &store, &cache).await?;

    let mut too_deep = backup::ChatItem {
        chat_id: 100,
        author_id: 2,
        date_sent: 12442118460000,
        ..Default::default()
    };
    for _ in 0..120 {
        too_deep = backup::ChatItem {
            revisions: vec![too_deep],
            ..Default::default()
        };
    }
    let mut newer = crew.to_vec();
    newer.push(backup::Frame {
        item: Some(backup::frame::Item::ChatItem(too_deep)),
    });
    write_frames(&snapshots, "signal-backup-2364-04-10-12-00-00", &newer)?;
    fetch_once(&snapshots, &store, &cache).await?;
    assert_eq!(ids(&store, "chat_items").await?.len(), 2);
    assert_eq!(
        problems(&store).await?,
        [("phase:frames".to_string(), "error".to_string())]
    );
    Ok(())
}

/// A backup folder that is gone is no backup at all: the run fails and
/// the store keeps what it had.
#[tokio::test(flavor = "multi_thread")]
async fn no_backup_to_read_deletes_nothing() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let snapshots = tmp.path().join("snapshots");
    let store = tmp.path().join("raw").join("signal");
    let cache = tmp.path().join("fingerprints.sqlite");
    let frames = [
        recipient_self(1, "Jean-Luc Picard"),
        recipient_contact(2, "Will Riker", 17015550101),
        chat_frame(100, 2),
        chat_item(100, 1, 12442118400000, "Status report.", true),
    ];
    write_frames(&snapshots, "signal-backup-2364-04-09-12-00-00", &frames)?;
    fetch_once(&snapshots, &store, &cache).await?;

    std::fs::remove_dir_all(&snapshots)?;
    assert!(fetch_once(&snapshots, &store, &cache).await.is_err());
    assert_eq!(ids(&store, "chat_items").await?.len(), 1);
    assert_eq!(ids(&store, "recipients").await?, ["1", "2"]);
    Ok(())
}

/// Reading an unchanged backup again must leave the store as it was: a
/// re-stamped sidecar is a commit, and a bigger store, on every sync.
#[tokio::test(flavor = "multi_thread")]
async fn reading_an_unchanged_backup_again_commits_nothing() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let snapshots = tmp.path().join("snapshots");
    let store = tmp.path().join("raw").join("signal");
    let cache = tmp.path().join("fingerprints.sqlite");
    let (_, plaintext_hash) = write_test_attachment(&snapshots.join("files"))?;
    let frames = [
        recipient_self(1, "Jean-Luc Picard"),
        recipient_contact(2, "Will Riker", 17015550101),
        chat_frame(100, 2),
        chat_item(100, 2, 12442118460000, "All decks at green status.", false),
        with_attachment(
            chat_item(100, 1, 12442118940000, "Make it so.", true),
            png_attachment(&plaintext_hash),
        ),
    ];
    write_frames(&snapshots, "signal-backup-2364-04-09-12-00-00", &frames)?;

    let first = fetch_with(&snapshots, &store, &cache, DownloadControl::default()).await?;
    let second = fetch_with(&snapshots, &store, &cache, DownloadControl::default()).await?;
    assert!(first.is_some());
    assert_eq!(
        second, None,
        "reading an unchanged backup again changes nothing in the store"
    );
    Ok(())
}

// ── A folder of snapshots ─────────────────────────────────────────────

const DAY_MS: u64 = 86_400_000;
/// 2364-04-11 00:00:00 UTC.
const APRIL_11_MS: u64 = 12442118400000;

/// The snapshot Signal would write `day` days after April 11th, named and
/// dated as Signal names and dates it.
fn write_day(root: &Path, day: u64, frames: &[backup::Frame]) -> Result<String> {
    let name = format!("signal-backup-2364-04-{:02}-00-00-00", 11 + day);
    write_frames_at(root, &name, APRIL_11_MS + day * DAY_MS, frames)?;
    Ok(name)
}

fn bridge_crew() -> Vec<backup::Frame> {
    vec![
        recipient_self(1, "Jean-Luc Picard"),
        recipient_contact(2, "Will Riker", 17015550101),
        chat_frame(100, 2),
    ]
}

/// `(first line of message, date)` of every snapshot commit, oldest first.
async fn backup_log(store: &Path) -> Result<Vec<(String, String)>> {
    let db = ingest::RawDb::open(&datalib_etl::doltlite_raw::db_path_for(store)).await?;
    let mut rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT message, date FROM dolt_log WHERE message LIKE 'download signal: backup %'",
    )
    .fetch_all(db.pool())
    .await?;
    db.close().await;
    rows.reverse();
    Ok(rows
        .into_iter()
        .map(|(m, d)| (m.lines().next().unwrap_or("").to_string(), d))
        .collect())
}

async fn texts(store: &Path) -> Result<Vec<String>> {
    let db = ingest::RawDb::open(&datalib_etl::doltlite_raw::db_path_for(store)).await?;
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT (SELECT value FROM json_tree(payload) WHERE key = 'body') FROM chat_items ORDER BY id",
    )
    .fetch_all(db.pool())
    .await?;
    db.close().await;
    Ok(rows)
}

/// Only the newest snapshot was read, so a folder of daily backups left
/// no history to diff: a message lost or changed between two backups was
/// invisible. Each is now a commit, dated when it was taken, and the
/// diff between two of them names what went and what changed.
#[tokio::test(flavor = "multi_thread")]
async fn each_backup_is_a_commit_and_two_diff_to_show_what_was_lost() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let snapshots = tmp.path().join("snapshots");
    let store = tmp.path().join("raw").join("signal");
    let cache = tmp.path().join("fingerprints.sqlite");

    let status = chat_item(100, 1, 12442118400000, "Status report.", true);
    let green = chat_item(100, 2, 12442118460000, "All decks at green status.", false);
    let vessel = chat_item(100, 2, 12442118520000, "Sensors detect a vessel.", false);
    let mut day0 = bridge_crew();
    day0.extend([status.clone(), green.clone()]);
    let mut day1 = day0.clone();
    day1.push(vessel);
    // Day two: Riker's green-status message is gone, and Picard's report
    // says something else under the same id.
    let mut day2 = bridge_crew();
    day2.push(chat_item(
        100,
        1,
        12442118400000,
        "Status report, Number One.",
        true,
    ));
    day2.push(chat_item(
        100,
        2,
        12442118520000,
        "Sensors detect a vessel.",
        false,
    ));
    // Written newest first, so directory order is not what orders them.
    let names = [
        write_day(&snapshots, 2, &day2)?,
        write_day(&snapshots, 0, &day0)?,
        write_day(&snapshots, 1, &day1)?,
    ];

    let (summary, moved) =
        fetch_summary(&snapshots, &store, &cache, DownloadControl::default()).await?;
    assert!(moved.is_some());
    assert_eq!(summary.backups_found, 3);
    assert_eq!(
        summary.backups_read,
        [names[1].as_str(), &names[2], &names[0]]
    );
    assert_eq!(
        backup_log(&store).await?,
        [
            (
                format!("download signal: backup {}", names[1]),
                "2364-04-11 00:00:00".to_string()
            ),
            (
                format!("download signal: backup {}", names[2]),
                "2364-04-12 00:00:00".to_string()
            ),
            (
                format!("download signal: backup {}", names[0]),
                "2364-04-13 00:00:00".to_string()
            ),
        ]
    );
    assert_eq!(
        texts(&store).await?,
        ["Status report, Number One.", "Sensors detect a vessel."],
        "HEAD holds the newest backup"
    );

    let db = ingest::RawDb::open(&datalib_etl::doltlite_raw::db_path_for(&store)).await?;
    let hashes: Vec<String> = sqlx::query_scalar(
        "SELECT commit_hash FROM dolt_log WHERE message LIKE 'download signal: backup %' \
          ORDER BY date",
    )
    .fetch_all(db.pool())
    .await?;
    let diff: Vec<(String, String)> = sqlx::query_as(
        "SELECT diff_type, coalesce(to_id, from_id) FROM dolt_diff_chat_items(?, ?) \
          ORDER BY 2",
    )
    .bind(&hashes[1])
    .bind(&hashes[2])
    .fetch_all(db.pool())
    .await?;
    // The bookkeeping of a row a day did not change is not written again,
    // so the day's commit holds only what changed.
    let restamped: i64 =
        sqlx::query_scalar("SELECT count(*) FROM dolt_diff_chat_items_bookkeeping(?, ?)")
            .bind(&hashes[0])
            .bind(&hashes[1])
            .fetch_one(db.pool())
            .await?;
    db.close().await;
    let id = |author: &str, sent: i64| ingest::schema_raw::chat_item_id_recipe("100", author, sent);
    assert_eq!(
        diff,
        [
            ("modified".to_string(), id("1", 12442118400000)),
            ("removed".to_string(), id("2", 12442118460000)),
        ]
    );
    assert_eq!(restamped, 1, "only the vessel message is new on day one");

    // Nothing new: the next run reads nothing and commits nothing.
    let (again, moved) =
        fetch_summary(&snapshots, &store, &cache, DownloadControl::default()).await?;
    assert!(again.backups_read.is_empty());
    assert_eq!(moved, None);
    Ok(())
}

/// A backup older than the newest one held, found late, is read like any
/// other; HEAD then returns to the newest, so the store never ends on
/// the past.
#[tokio::test(flavor = "multi_thread")]
async fn a_late_older_backup_is_read_and_the_newest_put_back_on_top() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let snapshots = tmp.path().join("snapshots");
    let store = tmp.path().join("raw").join("signal");
    let cache = tmp.path().join("fingerprints.sqlite");
    let mut newer = bridge_crew();
    newer.push(chat_item(100, 1, 12442118400000, "Make it so.", true));
    let newest = write_day(&snapshots, 2, &newer)?;
    fetch_once(&snapshots, &store, &cache).await?;

    let mut older = bridge_crew();
    older.push(chat_item(100, 2, 12442118460000, "Engage.", false));
    let late = write_day(&snapshots, 0, &older)?;
    let (summary, _) =
        fetch_summary(&snapshots, &store, &cache, DownloadControl::default()).await?;
    assert_eq!(summary.backups_read, [late.as_str(), &newest]);
    assert_eq!(texts(&store).await?, ["Make it so."]);
    let log = backup_log(&store).await?;
    assert_eq!(log.len(), 3, "{log:?}");
    assert!(
        log[2]
            .0
            .ends_with("read again to put the newest back on top"),
        "{log:?}"
    );

    // HEAD is on the newest now, so nothing is read again.
    let (summary, moved) =
        fetch_summary(&snapshots, &store, &cache, DownloadControl::default()).await?;
    assert!(summary.backups_read.is_empty());
    assert_eq!(moved, None);
    Ok(())
}

/// Reading an older backup on top of one that has left the folder would
/// delete from HEAD what only the newer one held, and read as data loss.
#[tokio::test(flavor = "multi_thread")]
async fn the_newest_backup_gone_from_the_folder_leaves_head_alone() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let snapshots = tmp.path().join("snapshots");
    let store = tmp.path().join("raw").join("signal");
    let cache = tmp.path().join("fingerprints.sqlite");
    let mut older = bridge_crew();
    older.push(chat_item(100, 1, 12442118400000, "Status report.", true));
    let mut newer = older.clone();
    newer.push(chat_item(
        100,
        2,
        12442118460000,
        "All decks at green status.",
        false,
    ));
    write_day(&snapshots, 0, &older)?;
    let gone = write_day(&snapshots, 1, &newer)?;
    fetch_once(&snapshots, &store, &cache).await?;

    std::fs::remove_dir_all(snapshots.join(&gone))?;
    fetch_once(&snapshots, &store, &cache).await?;
    assert_eq!(texts(&store).await?.len(), 2);
    assert_eq!(
        problems(&store).await?,
        [(
            format!("record:ingested_backups:{gone}"),
            "error".to_string()
        )]
    );
    Ok(())
}

/// One backup that will not open costs that backup, not the folder: the
/// rest are read, and it is retried on the next run.
#[tokio::test(flavor = "multi_thread")]
async fn a_backup_that_will_not_open_is_a_problem_and_the_rest_are_read() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let snapshots = tmp.path().join("snapshots");
    let store = tmp.path().join("raw").join("signal");
    let cache = tmp.path().join("fingerprints.sqlite");
    let mut frames = bridge_crew();
    frames.push(chat_item(100, 1, 12442118400000, "Status report.", true));
    let good = write_day(&snapshots, 0, &frames)?;
    let broken = write_day(&snapshots, 1, &frames)?;
    std::fs::write(snapshots.join(&broken).join("main"), b"not a backup")?;
    let unsynced = write_day(&snapshots, 2, &frames)?;
    std::fs::remove_file(snapshots.join(&unsynced).join("files"))?;

    let (summary, _) =
        fetch_summary(&snapshots, &store, &cache, DownloadControl::default()).await?;
    assert_eq!(summary.backups_read, [good.as_str()]);
    assert_eq!(
        problems(&store).await?,
        [
            (
                format!("record:ingested_backups:{broken}"),
                "error".to_string()
            ),
            (
                format!("record:ingested_backups:{unsynced}"),
                "error".to_string()
            ),
        ]
    );

    // The folder finishes syncing: that backup reads, and its problem goes.
    write_day(&snapshots, 2, &frames)?;
    fetch_once(&snapshots, &store, &cache).await?;
    assert_eq!(
        problems(&store).await?,
        [(
            format!("record:ingested_backups:{broken}"),
            "error".to_string()
        )]
    );
    Ok(())
}

/// A passphrase that opens none of the backups is not one problem per
/// backup: the run fails, and says why.
#[tokio::test(flavor = "multi_thread")]
async fn a_passphrase_that_opens_nothing_fails_the_run() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let snapshots = tmp.path().join("snapshots");
    let store = tmp.path().join("raw").join("signal");
    let cache = tmp.path().join("fingerprints.sqlite");
    write_day(&snapshots, 0, &bridge_crew())?;
    write_day(&snapshots, 1, &bridge_crew())?;

    let db = ingest::RawDb::open(&datalib_etl::doltlite_raw::db_path_for(&store)).await?;
    let run = ingest::fetch(FetchOptions {
        db: db.clone(),
        cache: FingerprintCache::open(&cache).await?,
        snapshot_root: snapshots.clone(),
        files_root: None,
        aep: "1".repeat(64),
        progress: Progress::noop(),
        control: DownloadControl::default(),
    })
    .await;
    db.close().await;
    let err = format!("{:#}", run.expect_err("no backup opens"));
    assert!(
        err.contains("no Signal backup this run tried would read"),
        "{err}"
    );
    Ok(())
}
