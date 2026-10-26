// Integration test runs under cargo-test (no MultiProgress / no
// indicatif bars). Exempt from the workspace-wide ban on direct
// stderr/stdout writes defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! Live Todoist download, and the check that a second run owes nothing.
//!
//! Not run by `bazel test`: every test here is named `live::<fn>`, and
//! the target skips that prefix. `bazel run
//! //datalib/backend/etl/providers/todoist:todoist_live` runs exactly
//! these, against the account latchkey holds for `todoist`
//! (`TODOIST_TEST_ACCOUNT` names one when it holds several). The store
//! is left on disk, and its path printed, for inspection; nothing in it
//! is printed here but counts.

use datalib_etl_todoist::ingest::{self, db_path_for, FetchSummary, RawDb};
use datalib_etl_todoist_config::TodoistSync;
use datalib_etl_web::http::LatchkeySettings;

async fn run(store: &std::path::Path, sync: &TodoistSync) -> FetchSummary {
    let db = RawDb::open(&db_path_for(store)).await.unwrap();
    let r = ingest::fetch(ingest::FetchOptions {
        latchkey: LatchkeySettings {
            account: std::env::var("TODOIST_TEST_ACCOUNT").ok(),
        },
        sync: sync.clone(),
        ..ingest::FetchOptions::new(db.clone())
    })
    .await;
    let sealed = datalib_etl::doltlite_raw::commit_run(db.pool(), "todoist_live").await;
    db.close().await;
    sealed.expect("seal the raw store");
    r.expect("todoist fetch")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_full_sync_then_a_second_run_that_reads_almost_nothing() {
    // A month of completed tasks keeps the first run short.
    let since = std::env::var("TODOIST_TEST_COMPLETED_SINCE").ok();
    let sync = TodoistSync {
        completed: true,
        completed_since: since,
    };
    let store = tempfile::TempDir::with_prefix("todoist-live-")
        .unwrap()
        .keep();
    eprintln!("[test] store: {}", store.display());

    let first = run(&store, &sync).await;
    eprintln!("[test] first run: {}", first.line());
    assert!(first.full_sync, "{first:?}");
    assert!(first.synced > 0, "{first:?}");

    let db = RawDb::open(&db_path_for(&store)).await.unwrap();
    let with_token: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM user WHERE json_extract(payload, '$.token') IS NOT NULL",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(with_token, 0, "the account's API token was stored");
    db.close().await;

    let second = run(&store, &sync).await;
    eprintln!("[test] second run: {}", second.line());
    assert!(!second.full_sync, "{second:?}");
    assert!(second.requests <= 3, "{second:?}");
}
