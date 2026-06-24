//! End-to-end mbox-mode test: run the extract::mbox extractor against
//! the checked-in Star Trek mbox fixture, then read it back via the
//! shared raw store and run it through `render_and_index_md::render::render_all`
//! — exercises the file-based ingest path end-to-end. Same code path
//! the sync orchestrator picks up when a `type: email` source has no
//! `sync:` block and `input_path` points at an `.mbox` file.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;

use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl_email::extract::db::{db_path_for, RawDb};
use frankweiler_etl_email::extract::mbox;
use frankweiler_etl_email::render_and_index_md::parse::parse;
use frankweiler_etl_email::render_and_index_md::render::{render_all, thread_uuid, OutlinkFormat};

fn fixture_path() -> PathBuf {
    if let Ok(dir) = std::env::var("JMAP_FIXTURE_DIR") {
        return PathBuf::from(dir).join("mbox/star_trek.mbox");
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mbox/star_trek.mbox")
}

async fn fetch_into_tmp(mbox_path: PathBuf) -> (tempfile::TempDir, PathBuf) {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("e.doltlite_db");
    let db = RawDb::open(&db_path).await.unwrap();
    let pool = db.pool().clone();
    mbox::fetch(mbox::FetchOptions {
        db_path: db_path.clone(),
        db: Some(db),
        input_path: mbox_path,
        account_id_override: Some("enterprise".to_string()),
        ..Default::default()
    })
    .await
    .expect("mbox extract fetch");
    // Close the writer pool so the subsequent reader-side open sees a
    // consistent doltlite working tree.
    pool.close().await;
    (tmp, db_path)
}

#[tokio::test(flavor = "multi_thread")]
async fn star_trek_mbox_lands_envelope_rows_and_joins() {
    let (_tmp, db_path) = fetch_into_tmp(fixture_path()).await;
    let db = RawDb::open(&db_path).await.unwrap();
    let emails = db.load_emails().await.unwrap();
    let threads = db.load_threads().await.unwrap();
    let mailboxes = db.load_mailboxes().await.unwrap();
    let joins = db.load_email_joins().await.unwrap();

    assert_eq!(emails.len(), 6);
    assert_eq!(threads.len(), 4);

    let briefing = threads
        .iter()
        .find(|t| t["id"] == "1000000000000000001")
        .expect("briefing thread present");
    assert_eq!(briefing["emailIds"].as_array().unwrap().len(), 3);

    // Mailbox role mapping preserved.
    let by_name: HashMap<&str, &serde_json::Value> = mailboxes
        .iter()
        .map(|m| (m["name"].as_str().unwrap(), m))
        .collect();
    assert_eq!(by_name["Inbox"]["role"], "inbox");
    assert_eq!(by_name["Sent"]["role"], "sent");
    assert!(by_name["Category Promotions"].get("role").is_none());

    // Geordi's message has the attachment.
    let geordi = emails
        .iter()
        .find(|e| e.id == "briefing-003@enterprise.starfleet")
        .expect("geordi present");
    assert!(geordi.has_attachment);
    // Attachment payloads now live inside the `.eml` itself (see
    // `extract_attachments_from_emls` at parse time) — not in a
    // dedicated email_attachments row. We only assert the
    // `has_attachment` flag here; the rendered-blobs test exercises
    // the parse-time mail-parsing path.

    // Geordi tagged Unread → no $seen (and possibly no keyword row at all if
    // Unread was the only label that mapped to a keyword).
    let geordi_kws = joins.keywords.get(&geordi.id).cloned().unwrap_or_default();
    assert!(!geordi_kws.iter().any(|k| k == "$seen"));
    let hayes = emails
        .iter()
        .find(|e| e.id == "briefing-001@enterprise.starfleet")
        .unwrap();
    let kws = &joins.keywords[&hayes.id];
    assert!(kws.iter().any(|k| k == "$flagged"));
    assert!(kws.iter().any(|k| k == "$important"));
    assert!(kws.iter().any(|k| k == "$seen"));

    // .eml bytes are in the CAS keyed by the email_blobs edge's blake3
    // (which mbox sets to the .eml's content hash at flush time).
    let blake3: Option<String> =
        sqlx::query_scalar("SELECT blake3 FROM email_blobs WHERE email_id = ?")
            .bind(&hayes.id)
            .fetch_one(db.pool())
            .await
            .unwrap();
    let blake3 = blake3.expect("email_blobs.blake3 set by mbox flush");
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM cas_objects WHERE blake3 = ?)")
            .bind(&blake3)
            .fetch_one(db.cas().pool())
            .await
            .unwrap();
    assert!(exists);

    // Re-running is idempotent: same email ids on a second pass.
    let pool = db.pool().clone();
    drop(db);
    pool.close().await;
    let db2 = RawDb::open(&db_path).await.unwrap();
    let pool2 = db2.pool().clone();
    mbox::fetch(mbox::FetchOptions {
        db_path: db_path.clone(),
        db: Some(db2),
        input_path: fixture_path(),
        account_id_override: Some("enterprise".to_string()),
        ..Default::default()
    })
    .await
    .unwrap();
    pool2.close().await;
    let db = RawDb::open(&db_path).await.unwrap();
    let emails2 = db.load_emails().await.unwrap();
    let ids1: Vec<_> = emails.iter().map(|e| &e.id).collect();
    let ids2: Vec<_> = emails2.iter().map(|e| &e.id).collect();
    assert_eq!(ids1, ids2);
}

/// Extract-time label filter: `only_labels` keeps only messages
/// carrying a matching `X-Gmail-Labels` label. Two of the six fixture
/// messages are filed under `Sent` (`Inbox,Sent` and `Sent,Important`),
/// so the filter lands exactly those two — and both end up in the `Sent`
/// mailbox.
#[tokio::test(flavor = "multi_thread")]
async fn mbox_only_labels_filters_extraction() {
    let tmp = tempfile::tempdir().unwrap();
    let db_path = tmp.path().join("e.doltlite_db");
    let db = RawDb::open(&db_path).await.unwrap();
    let pool = db.pool().clone();
    mbox::fetch(mbox::FetchOptions {
        db_path: db_path.clone(),
        db: Some(db),
        input_path: fixture_path(),
        account_id_override: Some("enterprise".to_string()),
        only_labels: vec!["Sent".to_string()],
        ..Default::default()
    })
    .await
    .expect("mbox extract fetch with label filter");
    pool.close().await;

    let db = RawDb::open(&db_path).await.unwrap();
    let emails = db.load_emails().await.unwrap();
    let mailboxes = db.load_mailboxes().await.unwrap();
    let joins = db.load_email_joins().await.unwrap();

    // Only the two Sent-labeled messages survived (vs. 6 unfiltered).
    assert_eq!(emails.len(), 2, "only Sent-labeled messages should land");

    // Every retained email is filed under the Sent mailbox.
    let sent_id = mailboxes
        .iter()
        .find(|m| m["name"] == "Sent")
        .expect("Sent mailbox present")["id"]
        .as_str()
        .unwrap()
        .to_string();
    for em in &emails {
        let mboxes = joins.mailboxes.get(&em.id).cloned().unwrap_or_default();
        assert!(
            mboxes.contains(&sent_id),
            "retained email {} not in Sent mailbox",
            em.id
        );
    }
}

/// Render-time label filter: extract everything, then render only the
/// threads touching a label. Thread-level inclusion — a whole thread
/// renders if any of its emails is filed under an allowed mailbox. The
/// expected thread set is computed from the parsed joins so the
/// assertion stays correct regardless of fixture threading details; it
/// must be a non-empty strict subset of the full set to be meaningful.
#[tokio::test(flavor = "multi_thread")]
async fn render_only_labels_filters_to_thread_subset() {
    let (_tmp_extract, db_path) = fetch_into_tmp(fixture_path()).await;
    let parsed = parse(&db_path_for(&db_path), None).expect("parse cold start");

    // "Sent" is a flat mbox label, so its full path equals its name.
    let sent_id = parsed
        .mailboxes
        .iter()
        .find(|m| m["name"] == "Sent")
        .expect("Sent mailbox present")["id"]
        .as_str()
        .unwrap()
        .to_string();
    let expected: HashSet<String> = parsed
        .docs
        .iter()
        .filter(|b| {
            b.emails.iter().any(|e| {
                b.joins
                    .mailboxes
                    .get(&e.id)
                    .is_some_and(|ids| ids.contains(&sent_id))
            })
        })
        .map(|b| thread_uuid(&b.account_id, &b.thread_id))
        .collect();
    assert!(!expected.is_empty(), "fixture should have a Sent thread");
    assert!(
        expected.len() < parsed.docs.len(),
        "filter must be a strict subset to be a meaningful test"
    );

    let tmp = tempfile::tempdir().unwrap();
    let progress = Progress::noop();
    let mut docs: Vec<RenderedMarkdown> = Vec::new();
    render_all(
        &parsed,
        tmp.path(),
        "star-trek-mbox",
        Some(OutlinkFormat::Gmail),
        &["Sent".to_string()],
        &progress,
        &mut |doc| {
            docs.push(doc);
            Ok(())
        },
    )
    .expect("render_all with render-label filter");

    let rendered: HashSet<String> = docs.iter().map(|d| d.markdown_uuid.clone()).collect();
    assert_eq!(rendered, expected, "rendered exactly the Sent threads");
}

#[tokio::test(flavor = "multi_thread")]
async fn star_trek_mbox_renders_through_render_all() {
    let (_tmp_extract, db_path) = fetch_into_tmp(fixture_path()).await;
    let parsed = parse(&db_path_for(&db_path), None).expect("parse cold start");

    let tmp = tempfile::tempdir().unwrap();
    let progress = Progress::noop();
    let mut docs: Vec<RenderedMarkdown> = Vec::new();
    render_all(
        &parsed,
        tmp.path(),
        "star-trek-mbox",
        Some(OutlinkFormat::Gmail),
        &[],
        &progress,
        &mut |doc| {
            docs.push(doc);
            Ok(())
        },
    )
    .expect("render_all");

    // One rendered document per thread.
    assert_eq!(docs.len(), 4);

    // chat-common owns the page-dir layout; locate each thread's page by
    // its markdown_uuid (= thread_uuid) from the captured RenderedMarkdowns.
    let dir_for = |tuid: &str| -> std::path::PathBuf {
        docs.iter()
            .find(|d| d.markdown_uuid == tuid)
            .unwrap_or_else(|| panic!("no rendered doc for {tuid}"))
            .md_path
            .parent()
            .unwrap()
            .to_path_buf()
    };

    // Briefing thread has the attachment materialized.
    let briefing_tuid = thread_uuid("enterprise", "1000000000000000001");
    let briefing_dir = dir_for(&briefing_tuid);
    let blobs_dir = briefing_dir.join("blobs");
    assert!(blobs_dir.is_dir(), "blobs/ dir missing");
    let mut found = false;
    for entry in std::fs::read_dir(&blobs_dir).unwrap().flatten() {
        if entry.path().is_file() {
            if let Ok(body) = std::fs::read_to_string(entry.path()) {
                if body.contains("Plasma flow") {
                    found = true;
                    break;
                }
            }
        }
    }
    assert!(
        found,
        "no attachment in {} contained Plasma flow",
        blobs_dir.display()
    );

    // Briefing md mentions every sender — names come from mail-parsing the
    // .eml in CAS, not from a server-pre-decoded payload.
    let md = std::fs::read_to_string(briefing_dir.join("all.md")).unwrap();
    for needle in ["Admiral Hayes", "Picard", "Geordi"] {
        assert!(md.contains(needle), "expected `{}` in briefing md", needle);
    }
    // Gmail outlink: rfc822msgid search built from each email's Message-ID.
    assert!(
        md.contains("https://mail.google.com/mail/u/0/#search/rfc822msgid:"),
        "expected a Gmail outlink in briefing md:\n{md}"
    );

    // Risa promo thread prefers the HTML body — `**jewel of the
    // Alpha Quadrant**` appears in the htmd output.
    let risa_tuid = thread_uuid("enterprise", "2000000000000000002");
    let risa_md = std::fs::read_to_string(dir_for(&risa_tuid).join("all.md")).unwrap();
    assert!(
        risa_md.contains("jewel of the Alpha Quadrant"),
        "expected html-rendered body in risa thread"
    );

    // Bridge-status thread has a `multipart/related` with an inline
    // PNG referenced as `<img src="cid:lcars-glyph@enterprise">`. The
    // renderer should materialize the PNG (injected into the per-thread
    // BlobBundle) and rewrite the cid to a `blobs/<hash>.png` link —
    // regression test for the Fastmail JMAP case where the inline image
    // isn't in the `attachments` array but is in the .eml MIME tree.
    let bridge_tuid = thread_uuid("enterprise", "4000000000000000004");
    let bridge_dir = dir_for(&bridge_tuid);
    let bridge_md = std::fs::read_to_string(bridge_dir.join("all.md")).unwrap();
    let bridge_blobs = bridge_dir.join("blobs");
    assert!(bridge_blobs.is_dir(), "bridge thread blobs/ dir missing");
    let png_files: Vec<_> = std::fs::read_dir(&bridge_blobs)
        .unwrap()
        .flatten()
        .filter(|e| {
            e.path()
                .extension()
                .and_then(|s| s.to_str())
                .map(|x| x == "png")
                .unwrap_or(false)
        })
        .collect();
    assert_eq!(
        png_files.len(),
        1,
        "expected exactly one .png in {}",
        bridge_blobs.display()
    );
    let png_name = png_files[0].file_name().to_string_lossy().into_owned();
    assert!(
        bridge_md.contains(&format!("(blobs/{png_name})")),
        "expected `blobs/{png_name}` markdown link in bridge thread, got:\n{bridge_md}"
    );
    // The raw cid URL should be gone — htmd should see the rewritten
    // `src="blobs/…"` and emit `![…](blobs/…)`, not `cid:…`.
    assert!(
        !bridge_md.contains("cid:lcars-glyph"),
        "raw cid: reference leaked into bridge thread md:\n{bridge_md}"
    );
}
