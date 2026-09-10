//! End-to-end test for the "SMS Backup & Restore" provider.

use datalib_etl::fingerprint_cache::FingerprintCache;
use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use datalib_etl::progress::Progress;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_sms_backup_restore::ingest::{self, db_path_for, FetchOptions, RawDb};
use datalib_etl_sms_backup_restore_render::render;

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
        // ── download ──────────────────────────────────────────────
        // The test owns the store: one connection for both downloads and
        // the assertions, because two is what breaks a doltlite file.
        let db = RawDb::open(&db_path_for(&raw_dir)).await?;
        let summary = ingest::fetch(FetchOptions {
            db: db.clone(),
            input_path: fixture_root(),
            cache: FingerprintCache::open(&tmp.path().join("fpcache.sqlite")).await?,
            progress: Progress::noop(),
            control: Default::default(),
        })
        .await
        .context("fetch")?;
        // Commit, the way the processor's `RawStoreSession` does in
        // production: render reads committed state only.
        datalib_etl::doltlite_raw::commit_run(db.pool(), "test: sms fetch").await?;

        assert_eq!(summary.files, 2, "2 xml files (sms + calls)");
        assert_eq!(summary.sms, 3, "3 plain SMS");
        assert_eq!(summary.mms, 3, "3 MMS");
        assert_eq!(summary.calls, 3, "3 calls");
        assert_eq!(summary.attachments, 3, "png + m4a + gif");
        assert_eq!(summary.blobs_stored, 3, "3 distinct blobs in CAS");
        assert_eq!(summary.parse_errors, 0);

        // 3 sms + 3 mms all land in one entity table.
        assert_eq!(
            db.load_payloads(datalib_etl::pin::Reads::Own, "sms_messages")
                .await?
                .len(),
            6
        );
        assert_eq!(
            db.load_payloads(datalib_etl::pin::Reads::Own, "sms_calls")
                .await?
                .len(),
            3
        );

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
        // A second pass over the unchanged files is a no-op: both
        // hash to what the cursor already stamped, so nothing
        // re-ingests — and the host cache answers without re-reading
        // a byte, because neither file's stat moved.
        let again = ingest::fetch(FetchOptions {
            db: db.clone(),
            input_path: fixture_root(),
            cache: FingerprintCache::open(&tmp.path().join("fpcache.sqlite")).await?,
            progress: Progress::noop(),
            control: Default::default(),
        })
        .await
        .context("second fetch")?;
        assert_eq!(again.files, 0, "unchanged files are skipped on re-run");
        assert_eq!(
            db.load_payloads(datalib_etl::pin::Reads::Own, "sms_messages")
                .await?
                .len(),
            6,
            "no duplicate messages after re-ingest"
        );

        // ── render ───────────────────────────────────────────────
        // `render` opens the store itself, so hand the file over first:
        // one doltlite file takes one connection at a time.
        db.close().await;
        let out_dir = tmp.path().join("out");
        fs::create_dir_all(&out_dir)?;
        let mut docs: Vec<RenderedMarkdown> = Vec::new();
        {
            let mut on_doc = |d: RenderedMarkdown| {
                docs.push(d);
                Ok(())
            };
            render::render(
                &raw_dir,
                &out_dir,
                "sms_backup_restore",
                &Progress::noop(),
                &HashMap::new(),
                &mut on_doc,
                None,
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
