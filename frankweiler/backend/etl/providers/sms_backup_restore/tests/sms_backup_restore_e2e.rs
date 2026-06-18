//! End-to-end test for the "SMS Backup & Restore" provider.
//!
//! Points extract at the checked-in TNG export tree (sms + calls XML
//! with inline base64 image / audio attachments), asserts the landed
//! raw tables + CAS blobs and the resume cursor, then runs render and
//! asserts the merged per-number conversations + materialized
//! attachments.

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl_sms_backup_restore::extract::{self, db_path_for, FetchOptions, RawDb};
use frankweiler_etl_sms_backup_restore::translate;

fn fixture_root() -> PathBuf {
    // Under Bazel the fixture is staged into runfiles and pointed at by
    // `SMS_FIXTURE_DIR`; under cargo we fall back to the source tree.
    if let Ok(d) = std::env::var("SMS_FIXTURE_DIR") {
        return PathBuf::from(d);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sms_backup_restore_tng")
}

#[test]
fn ingests_and_renders_the_tng_export() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let raw_dir = tmp.path().join("raw");
    fs::create_dir_all(&raw_dir)?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;

    rt.block_on(async {
        // ── extract ──────────────────────────────────────────────
        let summary = extract::fetch(FetchOptions {
            db_path: raw_dir.clone(),
            db: None,
            input_path: fixture_root(),
            progress: Progress::noop(),
            control: Default::default(),
        })
        .await
        .context("fetch")?;

        assert_eq!(summary.files, 2, "2 xml files (sms + calls)");
        assert_eq!(summary.sms, 3, "3 plain SMS");
        assert_eq!(summary.mms, 3, "3 MMS");
        assert_eq!(summary.calls, 3, "3 calls");
        assert_eq!(summary.attachments, 3, "png + m4a + gif");
        assert_eq!(summary.blobs_stored, 3, "3 distinct blobs in CAS");
        assert_eq!(summary.parse_errors, 0);

        let db = RawDb::open(&db_path_for(&raw_dir)).await?;
        // 3 sms + 3 mms all land in one entity table.
        assert_eq!(db.load_payloads("sms_messages").await?.len(), 6);
        assert_eq!(db.load_payloads("sms_calls").await?.len(), 3);

        // CAS edge rows carry a blake3, and the bytes are in cas_objects.
        let edges: i64 =
            sqlx::query_scalar("SELECT count(*) FROM sms_attachments WHERE blake3 IS NOT NULL")
                .fetch_one(db.pool())
                .await?;
        assert_eq!(edges, 3, "3 attachment edges with blake3");
        let blobs: i64 = sqlx::query_scalar("SELECT count(*) FROM cas_objects")
            .fetch_one(db.cas().pool())
            .await?;
        assert_eq!(blobs, 3, "3 blobs stored");

        // ── resume cursor ────────────────────────────────────────
        // A second pass over the unchanged files is a no-op: the
        // (size, mtime) cursor skips both, and nothing re-ingests.
        let again = extract::fetch(FetchOptions {
            db_path: raw_dir.clone(),
            db: None,
            input_path: fixture_root(),
            progress: Progress::noop(),
            control: Default::default(),
        })
        .await
        .context("second fetch")?;
        assert_eq!(again.files, 0, "unchanged files are skipped on re-run");
        assert_eq!(
            db.load_payloads("sms_messages").await?.len(),
            6,
            "no duplicate messages after re-ingest"
        );

        // ── render ───────────────────────────────────────────────
        let out_dir = tmp.path().join("out");
        fs::create_dir_all(&out_dir)?;
        let mut docs: Vec<RenderedMarkdown> = Vec::new();
        {
            let mut on_doc = |d: RenderedMarkdown| {
                docs.push(d);
                Ok(())
            };
            translate::render(
                &raw_dir,
                &out_dir,
                "sms_backup_restore",
                &Progress::noop(),
                &HashMap::new(),
                &mut on_doc,
            )
            .context("render")?;
        }

        // Three conversations: Picard (texts + mms + call merged), Worf
        // (text + mms), Deanna Troi (call only). All April 2369 → one
        // month bucket each.
        assert_eq!(docs.len(), 3, "three conversations, got {}", docs.len());

        let all_md: String = docs
            .iter()
            .map(|d| fs::read_to_string(&d.md_path).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");

        // Texts + MMS captions render.
        assert!(all_md.contains("Make it so."), "inbound SMS body");
        assert!(all_md.contains("Aye, Captain."), "outbound SMS body");
        assert!(
            all_md.contains("The Enterprise approaches."),
            "image MMS caption"
        );
        assert!(
            all_md.contains("Captain's log, supplemental."),
            "audio MMS caption"
        );
        assert!(
            all_md.contains("Bat'leth practice at 0700."),
            "gif MMS caption"
        );

        // Attachments materialized + linked (image, audio, gif).
        assert!(
            all_md.contains("blobs/"),
            "attachments materialized into blobs/"
        );
        assert!(
            all_md.contains("<audio"),
            "audio recording renders an inline player"
        );

        // Calls fold in as system notes of every flavor.
        assert!(all_md.contains("Incoming call"), "incoming call note");
        assert!(all_md.contains("Missed call"), "missed call note");
        assert!(all_md.contains("Outgoing call"), "outgoing call note");
        // Picard's incoming call shows its duration (95s → 1:35).
        assert!(all_md.contains("1:35"), "call duration formatted");

        Ok::<_, anyhow::Error>(())
    })?;

    Ok(())
}
