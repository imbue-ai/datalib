//! End-to-end test for the "SMS Backup & Restore" provider.

use datalib_etl_files::fingerprint_cache::FingerprintCache;
use std::fs;
use std::path::PathBuf;

use anyhow::{Context, Result};
use datalib_etl::progress::Progress;
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::RawRange;
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
        // the assertions, because the file takes one writer at a time.
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
        datalib_etl::store_handle::RawStoreHandle::commit_all(&db, "test: sms fetch").await?;

        assert_eq!(summary.files, 2, "2 xml files (sms + calls)");
        assert_eq!(summary.sms, 3, "3 plain SMS");
        assert_eq!(summary.mms, 3, "3 MMS");
        assert_eq!(summary.calls, 3, "3 calls");
        assert_eq!(summary.attachments, 3, "png + m4a + gif");
        assert_eq!(summary.blobs_stored, 3, "3 distinct blobs in CAS");
        assert_eq!(summary.parse_errors, 0);

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
            db.load_payloads("sms_messages").await?.len(),
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
                &mut on_doc,
                RawRange::cold(),
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

/// #898: a backup file that is gone takes with it the records no other
/// file still holds, and only those. Snapshots overlap, so deleting an
/// older copy of the same messages deletes nothing.
#[test]
fn a_deleted_backup_takes_only_what_no_other_file_holds() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let raw_dir = tmp.path().join("raw");
    let input = tmp.path().join("input");
    fs::create_dir_all(&raw_dir)?;
    fs::create_dir_all(&input)?;
    let sms = fs::read(fixture_root().join("sms-2369041512000.xml"))?;
    fs::write(input.join("sms-2369041512000.xml"), &sms)?;
    fs::write(input.join("sms-2369040112000.xml"), &sms)?;
    fs::copy(
        fixture_root().join("calls-2369041512000.xml"),
        input.join("calls-2369041512000.xml"),
    )?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(async {
        let db = RawDb::open(&db_path_for(&raw_dir)).await?;
        let fetch = |db: &RawDb| {
            let db = db.clone();
            let input = input.clone();
            let cache = tmp.path().join("fpcache.sqlite");
            async move {
                ingest::fetch(FetchOptions {
                    db,
                    input_path: input,
                    cache: FingerprintCache::open(&cache).await?,
                    progress: Progress::noop(),
                    control: Default::default(),
                })
                .await
            }
        };
        let count = |db: &RawDb, table: &'static str| {
            let pool = db.pool().clone();
            async move {
                sqlx::query_scalar::<_, i64>(sqlx::AssertSqlSafe(format!(
                    "SELECT count(*) FROM {table}"
                )))
                .fetch_one(&pool)
                .await
            }
        };
        fetch(&db).await?;
        assert_eq!(count(&db, "sms_messages").await?, 6);
        assert_eq!(count(&db, "sms_calls").await?, 3);

        fs::remove_file(input.join("sms-2369040112000.xml"))?;
        let older_gone = fetch(&db).await?;
        assert_eq!(older_gone.files_removed, 1);
        assert_eq!(
            older_gone.removed, 0,
            "the newer backup holds every message"
        );
        assert_eq!(count(&db, "sms_messages").await?, 6);
        assert_eq!(count(&db, "sms_attachments").await?, 3);

        fs::remove_file(input.join("calls-2369041512000.xml"))?;
        let calls_gone = fetch(&db).await?;
        assert_eq!(calls_gone.removed, 3);
        assert_eq!(count(&db, "sms_calls").await?, 0);
        assert_eq!(count(&db, "sms_messages").await?, 6);

        // A backup rewritten in place is the whole of that snapshot: a
        // message the new one no longer carries was deleted.
        let path = input.join("sms-2369041512000.xml");
        let xml = fs::read_to_string(&path)?;
        let first_sms = xml.find("<sms ").expect("an <sms> element");
        let end = first_sms + xml[first_sms..].find("/>").expect("self-closing <sms>") + 2;
        fs::write(&path, format!("{}{}", &xml[..first_sms], &xml[end..]))?;
        let rewritten = fetch(&db).await?;
        assert_eq!(rewritten.removed, 1);
        assert_eq!(count(&db, "sms_messages").await?, 5);

        // Nothing is gone any more, so a quiet run reads nothing.
        let quiet = fetch(&db).await?;
        assert_eq!((quiet.files, quiet.files_removed), (0, 0));
        db.close().await;
        Ok::<_, anyhow::Error>(())
    })
}

/// A walk that could not read an entry cannot tell a deleted backup from
/// one it failed to see, so nothing is deleted.
#[cfg(unix)]
#[test]
fn a_walk_error_deletes_nothing() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let raw_dir = tmp.path().join("raw");
    let input = tmp.path().join("input");
    fs::create_dir_all(&raw_dir)?;
    fs::create_dir_all(&input)?;
    fs::copy(
        fixture_root().join("calls-2369041512000.xml"),
        input.join("calls-2369041512000.xml"),
    )?;

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(async {
        let db = RawDb::open(&db_path_for(&raw_dir)).await?;
        let cache = tmp.path().join("fpcache.sqlite");
        let opts = |db: &RawDb, cache: FingerprintCache| FetchOptions {
            db: db.clone(),
            input_path: input.clone(),
            cache,
            progress: Progress::noop(),
            control: Default::default(),
        };
        ingest::fetch(opts(&db, FingerprintCache::open(&cache).await?)).await?;

        fs::remove_file(input.join("calls-2369041512000.xml"))?;
        std::os::unix::fs::symlink(input.join("nowhere"), input.join("sms-dangling.xml"))?;
        let s = ingest::fetch(opts(&db, FingerprintCache::open(&cache).await?)).await?;
        assert_eq!(s.removed, 0);
        let calls: i64 = sqlx::query_scalar("SELECT count(*) FROM sms_calls")
            .fetch_one(db.pool())
            .await?;
        assert_eq!(calls, 3);
        db.close().await;
        Ok::<_, anyhow::Error>(())
    })
}

async fn fetch_dir(
    db: &RawDb,
    input: &std::path::Path,
    cache: &std::path::Path,
) -> Result<ingest::FetchSummary> {
    ingest::fetch(FetchOptions {
        db: db.clone(),
        input_path: input.to_path_buf(),
        cache: FingerprintCache::open(cache).await?,
        progress: Progress::noop(),
        control: Default::default(),
    })
    .await
}

async fn problems(db: &RawDb) -> Result<Vec<(String, String)>> {
    Ok(
        sqlx::query_as("SELECT scope_key, severity FROM problems ORDER BY scope_key")
            .fetch_all(db.pool())
            .await?,
    )
}

/// A backup file that would not parse was a log line and a count; the
/// rest of the export landed with nothing naming the file it lacked. Its
/// row stands, without the file being read again, until it changes.
#[test]
fn a_file_that_will_not_parse_is_a_problem_until_it_does() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let raw_dir = tmp.path().join("raw");
    let input = tmp.path().join("input");
    fs::create_dir_all(&raw_dir)?;
    fs::create_dir_all(&input)?;
    fs::copy(
        fixture_root().join("calls-2369041512000.xml"),
        input.join("calls-2369041512000.xml"),
    )?;
    let broken = input.join("sms-2369040112000.xml");
    fs::write(
        &broken,
        r#"<smses count="1"><sms address=+17015550101 /></smses>"#,
    )?;
    let cache = tmp.path().join("fpcache.sqlite");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(async {
        let db = RawDb::open(&db_path_for(&raw_dir)).await?;
        let unparsed = vec![(
            "file:sms_backup_restore/xml:sms-2369040112000.xml".to_string(),
            "error".to_string(),
        )];
        fetch_dir(&db, &input, &cache).await?;
        assert_eq!(problems(&db).await?, unparsed);

        let quiet = fetch_dir(&db, &input, &cache).await?;
        assert_eq!(
            (quiet.files, quiet.parse_errors),
            (0, 0),
            "a file that will not parse is not read again until it changes"
        );
        assert_eq!(problems(&db).await?, unparsed);

        fs::copy(fixture_root().join("sms-2369041512000.xml"), &broken)?;
        fetch_dir(&db, &input, &cache).await?;
        assert!(problems(&db).await?.is_empty());
        db.close().await;
        Ok::<_, anyhow::Error>(())
    })
}

/// An MMS part whose base64 would not decode vanished from the message,
/// with only a log line to say it had been there.
#[test]
fn an_mms_part_that_will_not_decode_is_a_problem_until_it_does() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let raw_dir = tmp.path().join("raw");
    let input = tmp.path().join("input");
    fs::create_dir_all(&raw_dir)?;
    fs::create_dir_all(&input)?;
    let mms = |data: &str| {
        format!(
            r#"<smses count="1">
  <mms date="2369041512000" msg_box="1" address="+17015550101" m_id="NCC-1701-D">
    <parts>
      <part seq="0" ct="image/gif" cl="image000001.gif" data="{data}" />
      <part seq="0" ct="text/plain" text="Shields up" />
    </parts>
  </mms>
</smses>"#
        )
    };
    let path = input.join("sms-2369041512000.xml");
    fs::write(&path, mms("%%% not base64 %%%"))?;
    let cache = tmp.path().join("fpcache.sqlite");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(async {
        let db = RawDb::open(&db_path_for(&raw_dir)).await?;
        fetch_dir(&db, &input, &cache).await?;
        let rows = problems(&db).await?;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert!(
            rows[0].0.starts_with("sms_attachments:") && rows[0].0.ends_with("/image000001.gif"),
            "{rows:?}"
        );
        assert_eq!(rows[0].1, "error");

        let gif = "R0lGODlhAQABAIAAAAAAAP///yH5BAEAAAAALAAAAAABAAEAAAIBRAA7";
        fs::write(&path, mms(gif))?;
        fetch_dir(&db, &input, &cache).await?;
        assert!(problems(&db).await?.is_empty());
        db.close().await;
        Ok::<_, anyhow::Error>(())
    })
}

/// A run that held deletions back stamped the rewritten file anyway, so
/// the next run saw nothing rewritten, read only the new file, deleted
/// nothing, and cleared the held-back row: what left the rewritten file
/// was never deleted.
#[test]
fn deletions_held_back_are_made_once_every_file_reads() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let raw_dir = tmp.path().join("raw");
    let input = tmp.path().join("input");
    fs::create_dir_all(&raw_dir)?;
    fs::create_dir_all(&input)?;
    let sms = input.join("sms-2369041512000.xml");
    fs::copy(fixture_root().join("sms-2369041512000.xml"), &sms)?;
    let cache = tmp.path().join("fpcache.sqlite");
    let messages = |db: &RawDb| {
        let pool = db.pool().clone();
        async move {
            sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sms_messages")
                .fetch_one(&pool)
                .await
        }
    };

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(async {
        let db = RawDb::open(&db_path_for(&raw_dir)).await?;
        fetch_dir(&db, &input, &cache).await?;
        assert_eq!(messages(&db).await?, 6);

        // One message leaves the backup, and a new file will not parse.
        let xml = fs::read_to_string(&sms)?;
        let first = xml.find("<sms ").expect("an <sms> element");
        let end = first + xml[first..].find("/>").expect("self-closing <sms>") + 2;
        fs::write(&sms, format!("{}{}", &xml[..first], &xml[end..]))?;
        let broken = input.join("sms-2369040112000.xml");
        fs::write(&broken, r#"<smses count="1"><sms address=+1701 /></smses>"#)?;
        let held = fetch_dir(&db, &input, &cache).await?;
        assert_eq!(held.removed, 0);
        assert!(problems(&db)
            .await?
            .iter()
            .any(|(k, _)| k == "listing:removed_records"));

        fs::write(&broken, r#"<smses count="0"></smses>"#)?;
        let caught_up = fetch_dir(&db, &input, &cache).await?;
        assert_eq!(caught_up.removed, 1);
        assert_eq!(messages(&db).await?, 5);
        assert!(problems(&db).await?.is_empty());
        db.close().await;
        Ok::<_, anyhow::Error>(())
    })
}

/// A backup rewritten to nothing (0 bytes, bytes that are not XML, or a
/// copy cut off before its end) read as a clean archive holding fewer
/// messages, and the prune deleted every message only it held. Each is a
/// problem on the file and deletes nothing.
#[test]
fn a_backup_that_is_recognizably_nothing_deletes_nothing() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let raw_dir = tmp.path().join("raw");
    let input = tmp.path().join("input");
    fs::create_dir_all(&raw_dir)?;
    fs::create_dir_all(&input)?;
    let sms = input.join("sms-2369041512000.xml");
    fs::copy(fixture_root().join("sms-2369041512000.xml"), &sms)?;
    let whole = fs::read_to_string(&sms)?;
    let cut_off = whole[..whole.rfind("<sms ").expect("an <sms> element")].to_string();
    let cache = tmp.path().join("fpcache.sqlite");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(async {
        let db = RawDb::open(&db_path_for(&raw_dir)).await?;
        let messages = |db: &RawDb| {
            let pool = db.pool().clone();
            async move {
                sqlx::query_scalar::<_, i64>("SELECT count(*) FROM sms_messages")
                    .fetch_one(&pool)
                    .await
            }
        };
        fetch_dir(&db, &input, &cache).await?;
        assert_eq!(messages(&db).await?, 6);

        for (what, bytes) in [
            ("an empty file", String::new()),
            (
                "bytes that are not XML",
                "\u{1}\u{2} not a backup".to_string(),
            ),
            ("a copy cut off before its end", cut_off),
        ] {
            fs::write(&sms, &bytes)?;
            let s = fetch_dir(&db, &input, &cache).await?;
            assert_eq!(s.removed, 0, "{what} deleted messages");
            assert_eq!(messages(&db).await?, 6, "{what} deleted messages");
            assert!(
                problems(&db)
                    .await?
                    .iter()
                    .any(|(k, _)| k == "file:sms_backup_restore/xml:sms-2369041512000.xml"),
                "{what} is a problem on the file"
            );
        }

        // An empty backup that says so is a backup of nothing.
        fs::write(&sms, r#"<?xml version='1.0' ?><smses count="0"></smses>"#)?;
        let emptied = fetch_dir(&db, &input, &cache).await?;
        assert_eq!(emptied.removed, 6);
        assert_eq!(messages(&db).await?, 0);
        assert!(problems(&db).await?.is_empty());
        db.close().await;
        Ok::<_, anyhow::Error>(())
    })
}

/// The files read were stamped in one transaction and the records no file
/// held were pruned in a later one. A run that failed between the two left
/// the rewritten file stamped as read, so no later run read every file
/// again and the record it dropped was never deleted.
#[test]
fn a_run_that_fails_before_its_prune_prunes_on_the_next() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let raw_dir = tmp.path().join("raw");
    let input = tmp.path().join("input");
    fs::create_dir_all(&raw_dir)?;
    fs::create_dir_all(&input)?;
    let sms = input.join("sms-2369041512000.xml");
    fs::copy(fixture_root().join("sms-2369041512000.xml"), &sms)?;
    let cache = tmp.path().join("fpcache.sqlite");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    rt.block_on(async {
        let db = RawDb::open(&db_path_for(&raw_dir)).await?;
        fetch_dir(&db, &input, &cache).await?;

        let xml = fs::read_to_string(&sms)?;
        let first = xml.find("<sms ").expect("an <sms> element");
        let end = first + xml[first..].find("/>").expect("self-closing <sms>") + 2;
        fs::write(&sms, format!("{}{}", &xml[..first], &xml[end..]))?;

        // The run fails at its prune, the way a crash there would end it.
        sqlx::query(
            "CREATE TRIGGER refuse_prune BEFORE DELETE ON sms_messages \
             BEGIN SELECT RAISE(ABORT, 'crash at the prune'); END",
        )
        .execute(db.pool())
        .await?;
        assert!(fetch_dir(&db, &input, &cache).await.is_err());
        sqlx::query("DROP TRIGGER refuse_prune")
            .execute(db.pool())
            .await?;

        fetch_dir(&db, &input, &cache).await?;
        let messages: i64 = sqlx::query_scalar("SELECT count(*) FROM sms_messages")
            .fetch_one(db.pool())
            .await?;
        assert_eq!(messages, 5, "the message the backup dropped is deleted");
        db.close().await;
        Ok::<_, anyhow::Error>(())
    })
}

/// Reading an unchanged export again must leave the store as it was: a
/// re-stamped sidecar is a commit, and a bigger store, on every sync.
#[test]
fn reading_an_unchanged_export_again_commits_nothing() -> Result<()> {
    let tmp = tempfile::tempdir()?;
    let raw_dir = tmp.path().join("raw");
    fs::create_dir_all(&raw_dir)?;
    let cache = tmp.path().join("fpcache.sqlite");
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async {
        let db = RawDb::open(&db_path_for(&raw_dir)).await?;
        let mut commits = Vec::new();
        for _ in 0..2 {
            fetch_dir(&db, &fixture_root(), &cache).await?;
            commits.push(datalib_etl::doltlite_raw::commit_run(db.pool(), "test").await?);
        }
        db.close().await;
        assert!(commits[0].is_some());
        assert_eq!(
            commits[1], None,
            "reading an unchanged export again changes nothing in the store"
        );
        Ok(())
    })
}
