//! A targeted `conv_uuids` fetch reports *why* it could not get a
//! conversation, and does not mistake a refusal for an absence.
//!
//! claude.ai answers a detail GET with 403 routinely and transiently, so
//! the two cases below differ only in whether the 403 persists past the
//! retries — which is exactly what makes reporting them the same thing
//! wrong.

use std::fs;
use std::time::Duration;

use datalib_etl::http::{fixture_key, HttpRequest, HttpResponse, HttpService, PLAYBACK_ENV};
use datalib_etl::synthesize::Synthesizer;
use datalib_etl_claude::ingest::{db_path_for, fetch, FetchOptions, FetchSummary, RawDb};
use datalib_etl_claude::synthesize::{ClaudeSynth, BASE, DETAIL_QUERY};
use serde_json::json;
use tempfile::tempdir;

const ORG: &str = "org-a";

fn seed(api: &std::path::Path, playback: &std::path::Path) {
    fs::create_dir_all(api).unwrap();
    fs::write(
        api.join("conversations.json"),
        serde_json::to_vec_pretty(&json!([{
            "uuid": "c1",
            "name": "First",
            "updated_at": "2025-01-02T00:00:00Z",
            "organization_uuid": ORG,
            "account": {"uuid": "acct-1"},
            "chat_messages": [],
            "_source": {"via": "claude.ai/api", "org_uuid": ORG},
        }]))
        .unwrap(),
    )
    .unwrap();
    ClaudeSynth::new(api).synthesize(playback).unwrap();
}

/// Replace the detail fixture for `c1` with `status`.
///
/// The `Accept` header is not decoration: `fixture_key` hashes headers,
/// so without it this writes a file nothing reads and the test passes
/// against the untouched 200.
fn detail_answers(playback: &std::path::Path, status: u16) {
    let url = format!("{BASE}/organizations/{ORG}/chat_conversations/c1?{DETAIL_QUERY}");
    let req = HttpRequest::get(HttpService::Claude, &url).header("Accept", "application/json");
    let path = playback
        .join(HttpService::Claude.as_str())
        .join(fixture_key(&req));
    assert!(
        path.exists(),
        "no synthesized fixture at {} — this would overwrite nothing",
        path.display(),
    );
    let resp = HttpResponse {
        status,
        headers: Default::default(),
        body: b"{\"error\":\"forbidden\"}".to_vec(),
        duration_ms: 0,
    };
    fs::write(&path, serde_json::to_vec_pretty(&resp).unwrap()).unwrap();
}

async fn run(raw: &std::path::Path, api: &std::path::Path) -> FetchSummary {
    let db = RawDb::open(&db_path_for(raw)).await.unwrap();
    let o = FetchOptions {
        export_dir: Some(api.to_path_buf()),
        overlap: 0,
        sleep_between: Duration::ZERO,
        conv_uuids: vec!["c1".to_string()],
        projects: false,
        ..FetchOptions::new(db.clone())
    };
    let s = fetch(o).await;
    db.close().await;
    s.unwrap()
}

/// One test, two scenarios, run in sequence.
///
/// `PLAYBACK_ENV` is process-global and each scenario points it at its
/// own fixture dir, so as separate `#[tokio::test]`s they race and one
/// clears the other's playback root mid-request.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refusal_and_an_absence_are_reported_differently() {
    a_persistent_403_is_reported_as_forbidden_not_missing().await;
    a_404_everywhere_is_still_reported_as_not_found().await;
}

/// A 403 that outlives the retries is a *refusal*, not an absence.
/// Reporting it as `not_found` sends the reader hunting for a deleted
/// chat when the fix is a credential.
async fn a_persistent_403_is_reported_as_forbidden_not_missing() {
    let d = tempdir().unwrap();
    let api = d.path().join("input_snapshot");
    let playback = d.path().join("playback");
    let raw = d.path().join("raw");
    fs::create_dir_all(&raw).unwrap();
    seed(&api, &playback);
    detail_answers(&playback, 403);

    std::env::set_var(PLAYBACK_ENV, &playback);
    let s = run(&raw, &api).await;
    std::env::remove_var(PLAYBACK_ENV);

    assert_eq!(s.fetched, 0, "the conversation was refused: {s:?}");
    assert_eq!(s.problems.len(), 1, "{:?}", s.problems);
    assert_eq!(
        s.problems[0].reason.as_str(),
        "forbidden",
        "a refusal reported as {:?}",
        s.problems[0]
    );
    assert_eq!(s.problems[0].setting, "conv_uuids");
}

/// The other half, and the reason the distinction is not free: a 404
/// really does mean the id names nothing.
async fn a_404_everywhere_is_still_reported_as_not_found() {
    let d = tempdir().unwrap();
    let api = d.path().join("input_snapshot");
    let playback = d.path().join("playback");
    let raw = d.path().join("raw");
    fs::create_dir_all(&raw).unwrap();
    seed(&api, &playback);
    detail_answers(&playback, 404);

    std::env::set_var(PLAYBACK_ENV, &playback);
    let s = run(&raw, &api).await;
    std::env::remove_var(PLAYBACK_ENV);

    assert_eq!(s.fetched, 0);
    assert_eq!(s.problems.len(), 1, "{:?}", s.problems);
    assert_eq!(s.problems[0].reason.as_str(), "not_found");
}
