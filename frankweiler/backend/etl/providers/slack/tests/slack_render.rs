//! Golden test for `slack::render` against the TNG fixture.
//!
//! Renders the fixture into a tempdir, snapshots the per-thread `.md`
//! payloads, and verifies that a second render pass is a no-op (every
//! thread skipped via the `source_fingerprint` frontmatter match).

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use frankweiler_etl_slack::translate::render::render_all;
use frankweiler_etl_slack::translate::{translate_raw_dir, Message};
use insta::assert_snapshot;
use serde_json::json;

fn fixture_root() -> PathBuf {
    // Bazel sets `SLACK_FIXTURE_DIR` to a runfiles-relative path and
    // stages the fixture there via `data = [":tng_fixture"]`. Cargo
    // falls back to the on-disk package source.
    if let Ok(d) = std::env::var("SLACK_FIXTURE_DIR") {
        return PathBuf::from(d);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/slack_api")
}

fn collect_md(root: &std::path::Path) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut BTreeMap<String, String>) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, root, out);
            } else if p.extension().and_then(|s| s.to_str()) == Some("md") {
                let rel = p.strip_prefix(root).unwrap().to_string_lossy().to_string();
                out.insert(rel, fs::read_to_string(&p).unwrap());
            }
        }
    }
    walk(root, root, &mut out);
    out
}

#[test]
fn renders_tng_fixture_and_is_idempotent() {
    let t = translate_raw_dir(&fixture_root()).expect("translate");
    let tmp = tempfile::tempdir().expect("tmp");
    // First pass: empty prior-fingerprints → renders every thread; we
    // capture each (uuid → fingerprint) from the callback so we can
    // feed them back in on the re-render pass.
    let mut priors: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let summary = render_all(
        &t,
        tmp.path(),
        "slack_api",
        &frankweiler_etl::progress::Progress::noop(),
        &std::collections::HashMap::new(),
        &std::collections::HashMap::new(),
        &mut |doc: frankweiler_etl::load::RenderedDoc| -> anyhow::Result<()> {
            priors.insert(doc.document_uuid.clone(), doc.source_fingerprint.clone());
            Ok(())
        },
    )
    .expect("render");
    assert_eq!(summary.threads_total, 6);
    assert_eq!(summary.threads_rendered, 6);
    assert_eq!(summary.threads_skipped, 0);
    assert_eq!(priors.len(), 6);

    // Idempotent re-render: same fingerprints → every thread skipped,
    // callback never fired.
    let summary2 = render_all(
        &t,
        tmp.path(),
        "slack_api",
        &frankweiler_etl::progress::Progress::noop(),
        &priors,
        &std::collections::HashMap::new(),
        &mut |_doc| panic!("callback fired despite fingerprint match"),
    )
    .expect("re-render");
    assert_eq!(summary2.threads_rendered, 0);
    assert_eq!(summary2.threads_skipped, 6);

    let md_tree = collect_md(tmp.path());
    let mut bundle = String::new();
    for (path, body) in &md_tree {
        bundle.push_str("=== ");
        bundle.push_str(path);
        bundle.push_str(" ===\n");
        bundle.push_str(body);
        bundle.push('\n');
    }
    assert_snapshot!("tng_md_tree", bundle);

    // Sidecar shape spot-check: one `.grid_rows.json` per `.md`,
    // valid JSON, contains the thread header.
    let mut sidecar_paths: Vec<PathBuf> = Vec::new();
    fn walk(dir: &std::path::Path, out: &mut Vec<PathBuf>) {
        for e in fs::read_dir(dir).unwrap().flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.to_string_lossy().ends_with(".grid_rows.json") {
                out.push(p);
            }
        }
    }
    walk(tmp.path(), &mut sidecar_paths);
    assert_eq!(sidecar_paths.len(), 6);
    let one: serde_json::Value =
        serde_json::from_str(&fs::read_to_string(&sidecar_paths[0]).unwrap()).unwrap();
    assert!(one
        .get("header")
        .and_then(|h| h.get("source_fingerprint"))
        .is_some());
    assert!(one.get("rows").and_then(|r| r.as_array()).is_some());
}

/// Realistic incrementality test for the new render contract.
///
/// First pass: render the TNG fixture, capture every doc's fingerprint
/// from the callback so we have a fresh `prior_fingerprints` map for
/// the second pass.
///
/// Mutate state: add a reply to one existing thread (changing only
/// that thread's `source_fingerprint`) and synthesize a whole new
/// thread on an existing channel.
///
/// Second pass: re-render with the captured priors. Assert:
///   * untouched threads → skipped (callback never fires).
///   * mutated thread → re-rendered (callback fires once, fingerprint
///     differs from prior).
///   * new thread → rendered (callback fires once, was not in
///     priors).
///   * disk: the new thread's `index.md` exists; the mutated
///     thread's `index.md` contains the new reply text.
#[test]
fn renders_only_changed_and_new_threads_on_resync() {
    let mut t = translate_raw_dir(&fixture_root()).expect("translate");
    let tmp = tempfile::tempdir().expect("tmp");

    // ── pass 1: render everything, capture priors ──────────────────
    let mut priors: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let summary1 = render_all(
        &t,
        tmp.path(),
        "slack_api",
        &frankweiler_etl::progress::Progress::noop(),
        &std::collections::HashMap::new(),
        &std::collections::HashMap::new(),
        &mut |doc: frankweiler_etl::load::RenderedDoc| -> anyhow::Result<()> {
            priors.insert(doc.document_uuid.clone(), doc.source_fingerprint.clone());
            Ok(())
        },
    )
    .expect("render pass 1");
    assert_eq!(summary1.threads_total, 6);
    assert_eq!(summary1.threads_rendered, 6);
    assert_eq!(summary1.threads_skipped, 0);
    assert_eq!(priors.len(), 6);

    // ── mutate t: pick an existing thread and append a new reply ───
    // Sort by (channel_id, ts) for a stable choice across runs; we
    // just need one valid thread to extend.
    let parent: Message = {
        let mut all: Vec<&Message> = t.messages.values().collect();
        all.sort_by(|a, b| {
            (a.channel_id.as_str(), a.ts.as_str()).cmp(&(b.channel_id.as_str(), b.ts.as_str()))
        });
        let p = all
            .iter()
            .copied()
            .find(|m| m.is_thread_root)
            .expect("at least one thread root")
            .clone();
        p
    };
    let mutated_thread_uuid = parent.thread_uuid();
    let new_reply_ts = "9000000001.000001";
    let new_reply_text = "new reply added between sync runs";
    let new_reply = Message {
        team_id: parent.team_id.clone(),
        channel_id: parent.channel_id.clone(),
        ts: new_reply_ts.into(),
        thread_ts: Some(parent.ts.clone()),
        effective_thread_ts: parent.ts.clone(),
        is_thread_root: false,
        user_id: parent.user_id.clone(),
        text: new_reply_text.into(),
        ts_iso: "2026-06-01T00:00:00.000Z".into(),
        raw_json: json!({
            "ts": new_reply_ts,
            "thread_ts": parent.ts,
            "text": new_reply_text,
            "user": parent.user_id.clone().unwrap_or_default(),
        }),
    };
    t.messages
        .insert((parent.channel_id.clone(), new_reply_ts.into()), new_reply);

    // ── mutate t: add a whole new thread in the same channel ───────
    let new_thread_ts = "9000000002.000002";
    let new_thread_text = "synthesized brand-new thread";
    let new_thread_root = Message {
        team_id: parent.team_id.clone(),
        channel_id: parent.channel_id.clone(),
        ts: new_thread_ts.into(),
        thread_ts: None,
        effective_thread_ts: new_thread_ts.into(),
        is_thread_root: true,
        user_id: parent.user_id.clone(),
        text: new_thread_text.into(),
        ts_iso: "2026-06-01T01:00:00.000Z".into(),
        raw_json: json!({
            "ts": new_thread_ts,
            "text": new_thread_text,
            "user": parent.user_id.clone().unwrap_or_default(),
        }),
    };
    let new_thread_uuid = new_thread_root.thread_uuid();
    t.messages.insert(
        (parent.channel_id.clone(), new_thread_ts.into()),
        new_thread_root,
    );

    // Sanity: the existence of a *new* uuid means we'll see it as a
    // fresh thread in the second render pass.
    assert!(
        !priors.contains_key(&new_thread_uuid),
        "fixture happened to already have our synthesized thread_uuid; \
         pick fresh ts in the test",
    );

    // ── pass 2: render with priors; only the two changed/new docs
    //           should hit the callback ─────────────────────────────
    let mut rendered_uuids: Vec<String> = Vec::new();
    let summary2 = render_all(
        &t,
        tmp.path(),
        "slack_api",
        &frankweiler_etl::progress::Progress::noop(),
        &priors,
        &std::collections::HashMap::new(),
        &mut |doc: frankweiler_etl::load::RenderedDoc| -> anyhow::Result<()> {
            // Mutated thread must produce a different fingerprint.
            if doc.document_uuid == mutated_thread_uuid {
                let old = priors.get(&doc.document_uuid).expect("prior present");
                assert_ne!(
                    old, &doc.source_fingerprint,
                    "mutated thread's fingerprint did not change",
                );
            }
            rendered_uuids.push(doc.document_uuid.clone());
            Ok(())
        },
    )
    .expect("render pass 2");

    rendered_uuids.sort();
    let mut expected: Vec<String> = vec![mutated_thread_uuid.clone(), new_thread_uuid.clone()];
    expected.sort();
    assert_eq!(
        rendered_uuids, expected,
        "pass 2 should re-render only the mutated thread and the new \
         thread; skipped the other {} unchanged ones",
        summary2.threads_skipped,
    );
    assert_eq!(summary2.threads_total, 7); // 6 original + 1 new
    assert_eq!(summary2.threads_rendered, 2);
    assert_eq!(summary2.threads_skipped, 5);

    // ── disk sanity ────────────────────────────────────────────────
    // New thread's index.md exists.
    let new_thread_md = collect_md(tmp.path())
        .into_iter()
        .find(|(p, _)| p.contains(&new_thread_uuid));
    assert!(
        new_thread_md.is_some(),
        "new thread's index.md should exist on disk",
    );
    assert!(
        new_thread_md.unwrap().1.contains(new_thread_text),
        "new thread's body should contain the synthesized text",
    );

    // Mutated thread's index.md picked up the new reply.
    let mutated_md = collect_md(tmp.path())
        .into_iter()
        .find(|(p, _)| p.contains(&mutated_thread_uuid))
        .expect("mutated thread's index.md should exist");
    assert!(
        mutated_md.1.contains(new_reply_text),
        "mutated thread's body should contain the new reply",
    );

    // ── third pass: feed pass-2's fingerprints back, expect zero
    //               re-renders — confirms the new contract converges
    //               to a stable steady state ─────────────────────────
    let mut priors2 = priors.clone();
    // Replace mutated thread's fingerprint; add new thread.
    // (Capture from pass 2's callback would be cleaner, but rebuilding
    // here keeps the test linear.)
    let mut capture: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let _ = render_all(
        &t,
        tmp.path(),
        "slack_api",
        &frankweiler_etl::progress::Progress::noop(),
        &std::collections::HashMap::new(),
        &std::collections::HashMap::new(),
        &mut |doc: frankweiler_etl::load::RenderedDoc| -> anyhow::Result<()> {
            capture.insert(doc.document_uuid.clone(), doc.source_fingerprint.clone());
            Ok(())
        },
    )
    .expect("priors-rebuild pass");
    priors2.extend(capture);
    let summary3 = render_all(
        &t,
        tmp.path(),
        "slack_api",
        &frankweiler_etl::progress::Progress::noop(),
        &priors2,
        &std::collections::HashMap::new(),
        &mut |_doc| panic!("steady-state should skip every doc"),
    )
    .expect("render pass 3");
    assert_eq!(summary3.threads_rendered, 0);
    assert_eq!(summary3.threads_skipped, 7);
}
