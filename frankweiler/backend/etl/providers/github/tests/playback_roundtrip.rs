//! End-to-end synth → playback → download round-trip for GitHub.
//!
//! Seeds a fake event-store (the JSONL shape the synthesizer reads from),
//! synthesizes HTTP fixtures over it, then drives `download::fetch`
//! against a fresh doltlite database with `FRANKWEILER_HTTP_PLAYBACK`
//! pointed at the synthesized tree. Asserts the rehydrated DB carries
//! the same upstream payload per key.
//!
//! One test per binary so the process-wide playback env var can't race.

use std::collections::HashMap;
use std::fs;
use std::time::Duration;

use frankweiler_etl::event_store::{diff_and_save, make_record};
use frankweiler_etl::http::PLAYBACK_ENV;
use frankweiler_etl::synthesize::Synthesizer;
use frankweiler_etl_github::download::{
    block_on_load_all, db_path_for, fetch, FetchOptions, ENTITY_ISSUE_COMMENT, ENTITY_PR,
    ENTITY_PR_REVIEW, ENTITY_PR_REVIEW_COMMENT, ENTITY_SELF,
};
use frankweiler_etl_github::synthesize::GithubSynth;
use serde_json::{json, Map, Value};
use tempfile::tempdir;

fn write_event(api: &std::path::Path, entity: &str, key: Map<String, Value>, raw: Value) {
    let rec = make_record(key, raw);
    diff_and_save(api, entity, &[rec], &HashMap::new(), |r| r.to_string()).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn github_synth_playback_extract_roundtrip() {
    let d = tempdir().unwrap();
    let api = d.path().join("input_events");
    let playback = d.path().join("playback");
    let out_db = d.path().join("out.doltlite_db");
    fs::create_dir_all(&api).unwrap();

    let mut k = Map::new();
    k.insert("user_id".into(), json!(42));
    write_event(
        &api,
        ENTITY_SELF,
        k,
        json!({"id": 42, "login": "octocat", "html_url": "https://github.com/octocat"}),
    );

    let repo = "octocat/hello";
    let num = 7u64;
    let pr_raw = json!({
        "number": num,
        "title": "T",
        "state": "open",
        "html_url": format!("https://github.com/{repo}/pull/{num}"),
        "head": {"sha": "abc", "ref": "br"},
        "base": {"sha": "def", "ref": "main"},
    });
    let mut k = Map::new();
    k.insert("repo_full_name".into(), json!(repo));
    k.insert("pr_number".into(), json!(num));
    write_event(&api, ENTITY_PR, k, pr_raw.clone());

    let ic_raw = json!({"id": 101, "body": "hi", "user": {"login": "alice"}});
    let mut k = Map::new();
    k.insert("repo_full_name".into(), json!(repo));
    k.insert("pr_number".into(), json!(num));
    k.insert("comment_id".into(), json!(101));
    write_event(&api, ENTITY_ISSUE_COMMENT, k, ic_raw.clone());

    let rev_raw = json!({"id": 202, "state": "APPROVED", "user": {"login": "bob"}});
    let mut k = Map::new();
    k.insert("repo_full_name".into(), json!(repo));
    k.insert("pr_number".into(), json!(num));
    k.insert("review_id".into(), json!(202));
    write_event(&api, ENTITY_PR_REVIEW, k, rev_raw.clone());

    let rc_raw = json!({"id": 303, "body": "nit", "user": {"login": "carol"}, "path": "x.rs"});
    let mut k = Map::new();
    k.insert("repo_full_name".into(), json!(repo));
    k.insert("pr_number".into(), json!(num));
    k.insert("comment_id".into(), json!(303));
    write_event(&api, ENTITY_PR_REVIEW_COMMENT, k, rc_raw.clone());

    let report = GithubSynth::new(&api).synthesize(&playback).unwrap();
    // 1 user + 3 scope searches + 1 PR detail + 3 list endpoints = 8
    assert_eq!(report.fixtures_written, 8);

    std::env::set_var(PLAYBACK_ENV, &playback);

    let summary = fetch(FetchOptions {
        db_path: out_db.clone(),
        full_sync: true,
        refresh_window_days: 0,
        sleep_between: Duration::ZERO,
        ..FetchOptions::default()
    })
    .await
    .unwrap();
    assert_eq!(summary.new_prs, 1);
    assert_eq!(summary.new_issue_comments, 1);
    assert_eq!(summary.new_reviews, 1);
    assert_eq!(summary.new_review_comments, 1);

    let raw = block_on_load_all(&db_path_for(&out_db)).expect("load db");
    let me = raw.self_identity.expect("self identity present");
    assert_eq!(me["id"], 42);
    assert_eq!(me["login"], "octocat");

    assert_eq!(raw.pull_requests.len(), 1);
    assert_eq!(raw.pull_requests[0].payload, pr_raw);
    assert_eq!(raw.issue_comments.len(), 1);
    assert_eq!(raw.issue_comments[0].payload, ic_raw);
    assert_eq!(raw.pr_reviews.len(), 1);
    assert_eq!(raw.pr_reviews[0].payload, rev_raw);
    assert_eq!(raw.pr_review_comments.len(), 1);
    assert_eq!(raw.pr_review_comments[0].payload, rc_raw);
}
