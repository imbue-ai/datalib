// Integration test runs under cargo-test (no MultiProgress / no
// indicatif bars). Exempt from the workspace-wide ban on direct
// stderr/stdout writes defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! A live Zoom download of the last few weeks of one user's meetings.
//!
//! Not run by `bazel test`: every test here is named `live::<fn>`, and
//! the target skips that prefix. `ZOOM_TEST_USER=<email> bazel run
//! //datalib/backend/etl/providers/zoom:zoom_live` runs it against the
//! account latchkey holds for `zoom` (`ZOOM_TEST_ACCOUNT` names a
//! latchkey account when there are several). The store is left on disk,
//! and its path printed, for inspection. Nothing here has been run
//! against a real account yet: INGEST.md lists what is unverified.

use chrono::{Duration, Utc};
use datalib_etl_web::http::LatchkeySettings;
use datalib_etl_zoom::ingest::{self, db_path_for, FetchSummary, RawDb};
use datalib_etl_zoom_config::ZoomApi;

async fn run(store: &std::path::Path, api: &ZoomApi) -> FetchSummary {
    let db = RawDb::open(&db_path_for(store)).await.unwrap();
    let r = ingest::fetch(ingest::FetchOptions {
        db: db.clone(),
        latchkey: LatchkeySettings {
            account: std::env::var("ZOOM_TEST_ACCOUNT").ok(),
        },
        api: api.clone(),
        blob_size_limit_bytes: None,
        today: Utc::now().date_naive(),
        progress: Default::default(),
        control: Default::default(),
        sealer: None,
    })
    .await;
    let sealed = datalib_etl::doltlite_raw::commit_run(db.pool(), "zoom_live").await;
    db.close().await;
    sealed.expect("seal the raw store");
    r.expect("zoom fetch")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_last_weeks_copy_whole_and_a_second_run_owes_nothing() {
    let since = (Utc::now() - Duration::days(30))
        .format("%Y-%m-%d")
        .to_string();
    let api = ZoomApi {
        user: std::env::var("ZOOM_TEST_USER").ok(),
        since: Some(since),
        ..Default::default()
    };
    let store = tempfile::TempDir::with_prefix("zoom-live-").unwrap().keep();
    eprintln!("[test] store: {}", store.display());

    let first = run(&store, &api).await;
    eprintln!("[test] first run: {}", first.line());
    assert_eq!(first.failed, 0, "{first:?}");

    let db = RawDb::open(&db_path_for(&store)).await.unwrap();
    let transcripts: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM recordings r, json_each(r.payload, '$.recording_files') f \
         WHERE json_extract(f.value, '$.file_type') = 'TRANSCRIPT'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    eprintln!("[test] transcripts listed: {transcripts}");
    db.close().await;

    let second = run(&store, &api).await;
    eprintln!("[test] second run: {}", second.line());
    assert_eq!(
        (second.files, second.participants, second.summaries),
        (0, 0, 0),
        "{second:?}"
    );
}
