// Integration test runs under cargo-test (no MultiProgress / no
// indicatif bars). Exempt from the workspace-wide ban on direct
// stderr/stdout writes defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! Live Calendly download, the one check of the shapes `INGEST.md` marks
//! as taken from the API reference.
//!
//! Not run by `bazel test`: every test here is named `live::<fn>`, and
//! the target skips that prefix. `bazel run
//! //datalib/backend/etl/providers/calendar:calendar_live` runs exactly
//! these, against the token latchkey holds for `calendly`
//! (`CALENDLY_TEST_ACCOUNT` names the latchkey account when there are
//! several). It reads, and writes nothing upstream. The store is left on
//! disk, and its path printed, for inspection; nothing it holds is
//! printed.

use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_calendar::ingest::calendly;
use datalib_etl_calendar::ingest::{db_path_for, FetchSummary, RawDb};
use datalib_etl_web::http::LatchkeySettings;

async fn run(store: &std::path::Path) -> FetchSummary {
    let db = RawDb::open(&db_path_for(store)).await.unwrap();
    let r = calendly::fetch(calendly::FetchOptions {
        db: db.clone(),
        window: None,
        latchkey: LatchkeySettings {
            account: std::env::var("CALENDLY_TEST_ACCOUNT").ok(),
        },
        progress: Default::default(),
        control: Default::default(),
        sealer: None,
    })
    .await;
    db.commit_all("calendly_live")
        .await
        .expect("seal the raw store");
    db.close().await;
    r.expect("calendly fetch")
}

async fn count(store: &std::path::Path, sql: &'static str) -> i64 {
    let db = RawDb::open(&db_path_for(store)).await.unwrap();
    let n = sqlx::query_scalar(sql).fetch_one(db.pool()).await.unwrap();
    db.close().await;
    n
}

/// Every meeting the listing names is stored with its invitees, the
/// replies carried what the reference says they do, and a second run
/// with nothing booked in between reads no invitees again.
#[tokio::test(flavor = "multi_thread")]
async fn meetings_and_invitees_land_and_a_second_run_owes_nothing() {
    let store = tempfile::TempDir::with_prefix("calendly-live-")
        .unwrap()
        .keep();
    eprintln!("[test] store: {}", store.display());

    let first = run(&store).await;
    eprintln!("[test] first run: {}", first.line());
    assert_eq!(first.errors, 0, "{first:?}");
    let events = count(&store, "SELECT count(*) FROM calendly_events").await;
    let invitees = count(&store, "SELECT count(*) FROM calendly_invitees").await;
    assert_eq!(events, invitees, "every meeting's invitees were read");
    let shapeless = count(
        &store,
        "SELECT count(*) FROM calendly_events WHERE start_time IS NULL \
         OR json_extract(payload, '$.event_memberships') IS NULL",
    )
    .await;
    assert_eq!(shapeless, 0, "a meeting without the fields render reads");
    let unanswered = count(
        &store,
        "SELECT count(*) FROM calendly_invitees, json_each(json_extract(payload, '$.invitees')) \
         WHERE json_type(value, '$.questions_and_answers') IS NOT 'array'",
    )
    .await;
    assert_eq!(unanswered, 0, "an invitee without `questions_and_answers`");

    let second = run(&store).await;
    eprintln!("[test] second run: {}", second.line());
    assert_eq!(
        (second.events_new, second.events_updated, second.errors),
        (0, 0, 0),
        "{second:?}"
    );
    let pages = (events as usize).div_ceil(100).max(1);
    assert_eq!(
        second.requests,
        1 + pages,
        "the user and the listing only: {second:?}"
    );
}
