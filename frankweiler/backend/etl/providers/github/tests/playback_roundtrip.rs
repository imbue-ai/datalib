//! End-to-end synth → playback → extract round-trip for GitHub.
//!
//! Seeds a fake event-store, synthesizes HTTP fixtures over it, then
//! drives `extract::fetch` against a fresh output directory with
//! `FRANKWEILER_HTTP_PLAYBACK` pointed at the synthesized tree. Asserts
//! the rehydrated event-store carries the same `raw` payload per key.
//!
//! One test per binary so the process-wide playback env var can't race.

use std::collections::HashMap;
use std::fs;
use std::time::Duration;

use frankweiler_etl::event_store::{diff_and_save, load_latest_by_key, make_record};
use frankweiler_etl::http::PLAYBACK_ENV;
use frankweiler_etl::synthesize::Synthesizer;
use frankweiler_etl_github::extract::{
    fetch, FetchOptions, ENTITY_ISSUE_COMMENT, ENTITY_PR, ENTITY_PR_REVIEW,
    ENTITY_PR_REVIEW_COMMENT, ENTITY_SELF,
};
use frankweiler_etl_github::synthesize::GithubSynth;
use serde_json::{json, Map, Value};
use tempfile::tempdir;

fn write_event(api: &std::path::Path, entity: &str, key: Map<String, Value>, raw: Value) {
    let rec = make_record(key, raw);
    diff_and_save(api, entity, &[rec], &HashMap::new(), |r| r.to_string()).unwrap();
}

fn raws_by_key<F: FnMut(&Value) -> String>(
    dir: &std::path::Path,
    entity: &str,
    key_of: F,
) -> HashMap<String, Value> {
    load_latest_by_key(dir, entity, key_of)
        .unwrap()
        .into_iter()
        .map(|(k, v)| (k, v.get("raw").cloned().unwrap_or(Value::Null)))
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn github_synth_playback_extract_roundtrip() {
    let d = tempdir().unwrap();
    let api = d.path().join("input_events");
    let playback = d.path().join("playback");
    let out = d.path().join("out_events");
    fs::create_dir_all(&api).unwrap();

    // self_identity
    let mut k = Map::new();
    k.insert("user_id".into(), json!(42));
    write_event(
        &api,
        ENTITY_SELF,
        k,
        json!({"id": 42, "login": "octocat", "html_url": "https://github.com/octocat"}),
    );

    // one PR
    let repo = "octocat/hello";
    let num = 7u64;
    let mut k = Map::new();
    k.insert("repo_full_name".into(), json!(repo));
    k.insert("pr_number".into(), json!(num));
    write_event(
        &api,
        ENTITY_PR,
        k,
        json!({
            "number": num,
            "title": "T",
            "state": "open",
            "html_url": format!("https://github.com/{repo}/pull/{num}"),
            "head": {"sha": "abc", "ref": "br"},
            "base": {"sha": "def", "ref": "main"},
        }),
    );

    // one issue comment
    let mut k = Map::new();
    k.insert("repo_full_name".into(), json!(repo));
    k.insert("pr_number".into(), json!(num));
    k.insert("comment_id".into(), json!(101));
    write_event(
        &api,
        ENTITY_ISSUE_COMMENT,
        k,
        json!({"id": 101, "body": "hi", "user": {"login": "alice"}}),
    );

    // one PR review
    let mut k = Map::new();
    k.insert("repo_full_name".into(), json!(repo));
    k.insert("pr_number".into(), json!(num));
    k.insert("review_id".into(), json!(202));
    write_event(
        &api,
        ENTITY_PR_REVIEW,
        k,
        json!({"id": 202, "state": "APPROVED", "user": {"login": "bob"}}),
    );

    // one PR review comment
    let mut k = Map::new();
    k.insert("repo_full_name".into(), json!(repo));
    k.insert("pr_number".into(), json!(num));
    k.insert("comment_id".into(), json!(303));
    write_event(
        &api,
        ENTITY_PR_REVIEW_COMMENT,
        k,
        json!({"id": 303, "body": "nit", "user": {"login": "carol"}, "path": "x.rs"}),
    );

    let report = GithubSynth::new(&api).synthesize(&playback).unwrap();
    // 1 user + 3 scope searches + 1 PR detail + 3 list endpoints = 8
    assert_eq!(report.fixtures_written, 8);

    std::env::set_var(PLAYBACK_ENV, &playback);

    let summary = fetch(FetchOptions {
        out_dir: out.clone(),
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

    let key_pr = |r: &Value| {
        format!(
            "{}#{}",
            r["repo_full_name"].as_str().unwrap_or(""),
            r["pr_number"]
        )
    };
    let key_cmt = |r: &Value| {
        format!(
            "{}#{}#{}",
            r["repo_full_name"].as_str().unwrap_or(""),
            r["pr_number"],
            r["comment_id"]
        )
    };
    let key_rev = |r: &Value| {
        format!(
            "{}#{}#{}",
            r["repo_full_name"].as_str().unwrap_or(""),
            r["pr_number"],
            r["review_id"]
        )
    };

    for (entity, kf) in [
        (ENTITY_PR, &key_pr as &dyn Fn(&Value) -> String),
        (ENTITY_ISSUE_COMMENT, &key_cmt as &dyn Fn(&Value) -> String),
        (ENTITY_PR_REVIEW, &key_rev as &dyn Fn(&Value) -> String),
        (
            ENTITY_PR_REVIEW_COMMENT,
            &key_cmt as &dyn Fn(&Value) -> String,
        ),
    ] {
        let want = raws_by_key(&api, entity, |r| kf(r));
        let got = raws_by_key(&out, entity, |r| kf(r));
        assert_eq!(got, want, "{entity} mismatch");
    }
}
