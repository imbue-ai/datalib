//! GitLab synth → playback → download round-trip.

use std::collections::HashMap;
use std::fs;
use std::time::Duration;

use datalib_etl::event_store::{diff_and_save, make_record};
use datalib_etl::http::PLAYBACK_ENV;
use datalib_etl::synthesize::Synthesizer;
use datalib_etl_gitlab::download::{
    block_on_load_all, db_path_for, fetch, FetchOptions, RawDb, ENTITY_DISCUSSION, ENTITY_MR,
    ENTITY_SELF,
};
use datalib_etl_gitlab::synthesize::GitlabSynth;
use datalib_etl_gitlab_render::render::parse_api_dir;
use serde_json::{json, Map, Value};
use tempfile::tempdir;

fn write_event(api: &std::path::Path, entity: &str, key: Map<String, Value>, raw: Value) {
    let rec = make_record(key, raw);
    diff_and_save(api, entity, &[rec], &HashMap::new(), |r| r.to_string()).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gitlab_synth_playback_extract_roundtrip() {
    let d = tempdir().unwrap();
    let api = d.path().join("input_events");
    let playback = d.path().join("playback");
    let out_db = d.path().join("out.doltlite_db");
    fs::create_dir_all(&api).unwrap();

    let mut k = Map::new();
    k.insert("user_id".into(), json!(7));
    write_event(
        &api,
        ENTITY_SELF,
        k,
        json!({"id": 7, "username": "tt", "web_url": "https://gitlab.com/tt"}),
    );

    let proj = "ns/proj";
    let iid: u64 = 12;
    let mr_raw = json!({
        "iid": iid,
        "web_url": format!("https://gitlab.com/{proj}/-/merge_requests/{iid}"),
        "state": "opened",
        "source_branch": "feat",
        "target_branch": "main",
    });
    let mut k = Map::new();
    k.insert("project_full_path".into(), json!(proj));
    k.insert("mr_iid".into(), json!(iid));
    write_event(&api, ENTITY_MR, k, mr_raw.clone());

    let disc_raw = json!({"id": "abc", "individual_note": false, "notes": [{"updated_at": "2025-01-01T00:00:00Z"}]});
    let mut k = Map::new();
    k.insert("project_full_path".into(), json!(proj));
    k.insert("mr_iid".into(), json!(iid));
    k.insert("discussion_id".into(), json!("abc"));
    write_event(&api, ENTITY_DISCUSSION, k, disc_raw.clone());

    let report = GitlabSynth::new(&api).synthesize(&playback).unwrap();
    assert_eq!(report.fixtures_written, 6);

    std::env::set_var(PLAYBACK_ENV, &playback);

    // The test owns the store: one connection for the download and the
    // assertions both, because two is what breaks a doltlite file.
    let db = RawDb::open(&db_path_for(&out_db)).await.unwrap();
    let summary = fetch(FetchOptions {
        db_path: out_db.clone(),
        full_sync: true,
        refresh_window_days: 0,
        sleep_between: Duration::ZERO,
        ..FetchOptions::new(db.clone())
    })
    .await;
    // Seal on the same handle, the way the download step's
    // `RawStoreSession::finish` does. The render read below is taken at a
    // commit, so without this it has nothing to read.
    datalib_etl::doltlite_raw::commit_run(db.pool(), "test: gitlab download")
        .await
        .expect("seal the raw store");
    db.close().await;
    let summary = summary.unwrap();
    assert_eq!(summary.new_mrs, 1);
    assert_eq!(summary.new_discussions, 1);

    // The render side of the seam, and it has to come first: render reads
    // somebody else's store at a commit, while `block_on_load_all` below
    // opens read-write and rescue-commits on the way in — so running that
    // first would seal the store and hide a missing seal. gitlab had no
    // test crossing this seam at all, which is how `gitlab_live` came to
    // read an unsealed store and assert on zero rows.
    let parsed = parse_api_dir(&out_db, None).expect("parse_api_dir");
    assert_eq!(
        parsed.merge_requests.len(),
        1,
        "render found no MR — a zero here means the download's rows were \
         never committed, not that the source is empty"
    );
    assert_eq!(parsed.merge_requests[0].mr_iid as u64, iid);

    let raw = block_on_load_all(&db_path_for(&out_db)).expect("load db");
    let me = raw.self_identity.expect("self identity present");
    assert_eq!(me["id"], 7);
    assert_eq!(me["username"], "tt");

    assert_eq!(raw.merge_requests.len(), 1);
    assert_eq!(raw.merge_requests[0].payload, mr_raw);
    assert_eq!(raw.discussions.len(), 1);
    assert_eq!(raw.discussions[0].payload, disc_raw);
}
