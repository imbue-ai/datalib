//! What the playback tests share: a tape of TNG pull requests served
//! request by request, one download into a store, and the store read
//! back.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use datalib_etl::event_store::{diff_and_save, make_record};
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_forge_ingest_common::Bounds;
use datalib_etl_github::ingest::{
    block_on_load_all, db_path_for, fetch, FetchOptions, FetchSummary, RawDb, ENTITY_PR,
    ENTITY_SELF,
};
use datalib_etl_github::synthesize::GithubSynth;
use datalib_etl_web::http::{fixture_key, HttpRequest, HttpService, PLAYBACK_ENV};
use datalib_etl_web::synthesize::Synthesizer;
use serde_json::{json, Map};

pub const REPO: &str = "enterprise-d/holodeck";
/// The top of what a run at the TNG clock covers, as search takes it:
/// where the next run's search starts.
pub const SINCE: &str = "2369-04-15";

pub fn resumed() -> Bounds {
    Bounds {
        lo: Some(SINCE.to_string()),
        hi: None,
    }
}

/// A playback tree for an account on PRs `prs` of [`REPO`], none with
/// comments, captured at the TNG clock — so it also answers the searches
/// a run resumed from that moment sends, with nothing.
pub fn tape(dir: &Path, prs: &[u64]) -> PathBuf {
    let api = dir.join("events");
    fs::create_dir_all(&api).unwrap();
    let mut k = Map::new();
    k.insert("user_id".into(), json!(17010001));
    let mut me = make_record(k, json!({"id": 17010001, "login": "jlpicard"}));
    me["_recorded_at"] = json!(crate::tng_now().to_rfc3339_secs());
    diff_and_save(&api, ENTITY_SELF, &[me], &HashMap::new(), |r| r.to_string()).unwrap();
    for num in prs {
        let mut k = Map::new();
        k.insert("repo_full_name".into(), json!(REPO));
        k.insert("pr_number".into(), json!(num));
        let pr = make_record(
            k,
            json!({"number": num, "title": "Safety protocols", "state": "open"}),
        );
        diff_and_save(&api, ENTITY_PR, &[pr], &HashMap::new(), |r| r.to_string()).unwrap();
    }
    let pb = dir.join("playback");
    GithubSynth::new(&api).synthesize(&pb).unwrap();
    pb
}

pub fn tape_of(pb: &Path, url: &str) -> PathBuf {
    let path = pb
        .join("github")
        .join(fixture_key(&HttpRequest::get(HttpService::Github, url)));
    assert!(
        path.is_file(),
        "no tape for {url}; if the request shape changed, this test would \
         stage nothing and pass for the wrong reason"
    );
    path
}

pub async fn run(
    out: &Path,
    pb: &Path,
    tweak: impl FnOnce(FetchOptions) -> FetchOptions,
) -> Result<FetchSummary, String> {
    std::env::set_var(PLAYBACK_ENV, pb);
    let db = RawDb::open(&db_path_for(out)).await.unwrap();
    let opts = FetchOptions {
        refresh_window_days: 0,
        ..FetchOptions::new(db.clone(), crate::tng_now())
    };
    let summary = fetch(tweak(opts)).await.map_err(|e| format!("{e:#}"));
    // As the processor does: a run that fails commits nothing, and the
    // next open of the store drops what it wrote.
    if summary.is_ok() {
        db.commit_all("test").await.unwrap();
    }
    db.close().await;
    summary
}

pub async fn query(out: &Path, sql: &'static str) -> Vec<(String, String)> {
    let db = RawDb::open(&db_path_for(out)).await.unwrap();
    let rows = sqlx::query_as(sql).fetch_all(db.pool()).await.unwrap();
    db.close().await;
    rows
}

pub async fn problems(out: &Path) -> Vec<(String, String)> {
    query(
        out,
        "SELECT scope_key, severity FROM problems ORDER BY scope_key",
    )
    .await
}

/// `(scope, hi)` of every span the searches have covered.
pub async fn coverage(out: &Path) -> Vec<(String, String)> {
    query(out, "SELECT scope, hi FROM coverage ORDER BY scope").await
}

/// `(id, updated_at)` of every PR the searches have listed.
pub async fn listed(out: &Path) -> Vec<(String, Option<String>)> {
    let db = RawDb::open(&db_path_for(out)).await.unwrap();
    let rows = sqlx::query_as("SELECT id, updated_at FROM listed_change_requests ORDER BY id")
        .fetch_all(db.pool())
        .await
        .unwrap();
    db.close().await;
    rows
}

pub fn stored_prs(out: &Path) -> Vec<u32> {
    let raw = block_on_load_all(&db_path_for(out)).unwrap();
    raw.pull_requests.iter().map(|p| p.pr_number).collect()
}

pub fn row(key: &str, severity: &str) -> (String, String) {
    (key.to_string(), severity.to_string())
}
