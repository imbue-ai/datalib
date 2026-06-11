//! End-to-end mbox-mode test: run the extract::mbox extractor against
//! the checked-in Star Trek mbox fixture, then read it back via the
//! shared raw store and run it through `translate::render::render_all`
//! — exercises the file-based ingest path end-to-end. Same code path
//! the sync orchestrator picks up when a `type: email` source has no
//! `sync:` block and `input_path` points at an `.mbox` file.

use std::collections::HashMap;
use std::path::PathBuf;

use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl_email::extract::db::{block_on_load_all, RawDb};
use frankweiler_etl_email::extract::mbox;
use frankweiler_etl_email::translate::render::{render_all, thread_uuid};

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

    assert_eq!(emails.len(), 5);
    assert_eq!(threads.len(), 3);

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
    let atts = &joins.attachments[&geordi.id];
    assert_eq!(atts.len(), 1);
    assert_eq!(atts[0].name.as_deref(), Some("warp_diagnostics.txt"));

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

    // .eml bytes are in the CAS keyed by blob_id.
    assert!(db.blob_exists(&hayes.blob_id).await.unwrap());

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

#[tokio::test(flavor = "multi_thread")]
async fn star_trek_mbox_renders_through_render_all() {
    let (_tmp_extract, db_path) = fetch_into_tmp(fixture_path()).await;
    let raw = block_on_load_all(&db_path).expect("load_all");

    let tmp = tempfile::tempdir().unwrap();
    let progress = Progress::noop();
    let mut docs: Vec<RenderedMarkdown> = Vec::new();
    render_all(
        &raw,
        tmp.path(),
        "star-trek-mbox",
        &progress,
        &HashMap::new(),
        &mut |doc| {
            docs.push(doc);
            Ok(())
        },
    )
    .expect("render_all");

    // One rendered document per thread.
    assert_eq!(docs.len(), 3);

    // Briefing thread has the attachment materialized.
    let briefing_tuid = thread_uuid("enterprise", "1000000000000000001");
    let briefing_dir = tmp
        .path()
        .join("rendered_md/jmap/enterprise")
        .join(&briefing_tuid);
    assert!(briefing_dir.join("index.md").exists());
    assert!(briefing_dir.join("index.grid_rows.json").exists());
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

    // Briefing index.md mentions every sender — names come from
    // mail-parsing the .eml in CAS, not from a server-pre-decoded
    // payload.
    let md = std::fs::read_to_string(briefing_dir.join("index.md")).unwrap();
    for needle in ["Admiral Hayes", "Picard", "Geordi"] {
        assert!(
            md.contains(needle),
            "expected `{}` in briefing index.md",
            needle
        );
    }

    // Risa promo thread prefers the HTML body — `**jewel of the
    // Alpha Quadrant**` appears in the html2md output.
    let risa_tuid = thread_uuid("enterprise", "2000000000000000002");
    let risa_md = std::fs::read_to_string(
        tmp.path()
            .join("rendered_md/jmap/enterprise")
            .join(&risa_tuid)
            .join("index.md"),
    )
    .unwrap();
    assert!(
        risa_md.contains("jewel of the Alpha Quadrant"),
        "expected html-rendered body in risa thread"
    );
}
