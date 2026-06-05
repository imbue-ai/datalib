//! End-to-end mbox-mode test: parse the checked-in Star Trek mbox
//! fixture and run it through `translate::render::render_all` —
//! exercises the file-based ingest path without needing a JMAP
//! server or a doltlite raw db.
//!
//! This is the same fixture the sync-orchestrator pipeline would
//! pick up if the user pointed a `type: jmap_api` source (with no
//! `sync:` block) at the mbox file.

use std::collections::HashMap;
use std::path::PathBuf;

use frankweiler_etl::load::RenderedMarkdown;
use frankweiler_etl::progress::Progress;
use frankweiler_etl_email::translate::mbox;
use frankweiler_etl_email::translate::render::{render_all, thread_uuid};

fn fixture_path() -> PathBuf {
    // Bazel sets JMAP_FIXTURE_DIR (relative to runfiles); cargo runs
    // tests from the crate root and we can fall back to the literal
    // path on disk.
    if let Ok(dir) = std::env::var("JMAP_FIXTURE_DIR") {
        return PathBuf::from(dir).join("mbox/star_trek.mbox");
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/mbox/star_trek.mbox")
}

#[test]
fn star_trek_mbox_renders_two_threads_with_expected_metadata() {
    let path = fixture_path();
    let raw = mbox::parse(&path, Some("enterprise")).expect("parse mbox");

    // Five emails total, across two threads (briefing thread = 3,
    // Risa promo = 1, captain's log = 1).
    assert_eq!(raw.emails.len(), 5);
    assert_eq!(raw.threads.len(), 3);

    // Account_id override flows through end-to-end.
    assert_eq!(raw.accounts[0]["id"], "enterprise");

    // Briefing thread groups three messages.
    let briefing = raw
        .threads
        .iter()
        .find(|t| t["id"] == "1000000000000000001")
        .expect("briefing thread present");
    assert_eq!(briefing["emailIds"].as_array().unwrap().len(), 3);

    // Stable ids: re-parsing the same bytes yields identical email ids
    // and blob ids.
    let raw2 = mbox::parse(&path, Some("enterprise")).expect("parse mbox 2");
    let ids1: Vec<_> = raw.emails.iter().map(|e| &e.id).collect();
    let ids2: Vec<_> = raw2.emails.iter().map(|e| &e.id).collect();
    assert_eq!(ids1, ids2);

    // Inbox mailbox has role=inbox; Sent has role=sent; Category
    // Promotions is a plain mailbox with no role.
    let by_name: HashMap<&str, &serde_json::Value> = raw
        .mailboxes
        .iter()
        .map(|m| (m["name"].as_str().unwrap(), m))
        .collect();
    assert_eq!(by_name["Inbox"]["role"], "inbox");
    assert_eq!(by_name["Sent"]["role"], "sent");
    assert!(by_name["Category Promotions"].get("role").is_none());

    // Geordi's message has the attachment.
    let geordi = raw
        .emails
        .iter()
        .find(|e| e.id == "briefing-003@enterprise.starfleet")
        .expect("geordi present");
    assert!(geordi.has_attachment);
    let atts = &raw.joins.attachments[&geordi.id];
    assert_eq!(atts.len(), 1);
    assert_eq!(atts[0].name.as_deref(), Some("warp_diagnostics.txt"));

    // Geordi's message was tagged Unread — no $seen.
    let kws = &raw.joins.keywords[&geordi.id];
    assert!(!kws.iter().any(|k| k == "$seen"));
    // Admiral Hayes' message was Starred + Important — both keywords.
    let hayes = raw
        .emails
        .iter()
        .find(|e| e.id == "briefing-001@enterprise.starfleet")
        .unwrap();
    let kws = &raw.joins.keywords[&hayes.id];
    assert!(kws.iter().any(|k| k == "$flagged"));
    assert!(kws.iter().any(|k| k == "$important"));
    assert!(kws.iter().any(|k| k == "$seen"));
}

#[test]
fn star_trek_mbox_renders_through_render_all() {
    let path = fixture_path();
    let raw = mbox::parse(&path, Some("enterprise")).expect("parse mbox");
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
    let warp_diag = briefing_dir.join("blobs/warp_diagnostics.txt");
    assert!(
        warp_diag.exists(),
        "expected attachment at {}",
        warp_diag.display()
    );
    let body = std::fs::read_to_string(warp_diag).unwrap();
    assert!(body.contains("Plasma flow"));

    // Briefing index.md mentions every sender.
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
