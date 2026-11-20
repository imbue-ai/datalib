//! The Google Calendar download against the fake Google's Calendar.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_calendar::ingest::google;
use datalib_etl_calendar::ingest::{db_path_for, FetchSummary, RawDb, Window};
use datalib_fake_services::{Fakes, GOOGLE};
use serde_json::{json, Value};

const TOKEN: &str = "ya29.fake-starfleet-google-token";
const PICARD: &str = "picard@enterprise.example";

/// Picard's calendar: a staff briefing with the senior staff and a
/// diplomatic reception, a week apart.
fn seed() -> Value {
    json!({
        "anchor": "2026-03-21T00:00:00Z",
        "time_shift": "none",
        "tokens": [{"token": TOKEN, "user": PICARD}],
        "users": [
            {"email": PICARD, "name": "Jean-Luc Picard"},
            {"email": "riker@enterprise.example", "name": "William Riker"},
        ],
        "events": [
            {"key": "staff-briefing", "owner": PICARD, "summary": "Senior staff briefing", "start": "2026-03-17T09:00:00-07:00", "end": "2026-03-17T10:00:00-07:00", "location": "Observation lounge", "attendees": [
                {"email": "riker@enterprise.example", "name": "William Riker", "response_status": "accepted"},
            ]},
            {"key": "reception", "owner": PICARD, "summary": "Diplomatic reception", "start": "2026-03-10T18:00:00-07:00", "end": "2026-03-10T21:00:00-07:00"},
        ]
    })
}

async fn fetch_into(store: &Path, window: Option<Window>) -> anyhow::Result<FetchSummary> {
    std::fs::create_dir_all(store)?;
    let db = RawDb::open(&db_path_for(store)).await?;
    let summary = google::fetch(google::FetchOptions {
        db: db.clone(),
        calendars: Vec::new(),
        window,
        latchkey: Default::default(),
        progress: Default::default(),
        control: Default::default(),
        sealer: None,
    })
    .await;
    if summary.is_ok() {
        db.commit_all("test").await?;
    }
    db.close().await;
    summary
}

async fn summaries(store: &Path) -> BTreeSet<String> {
    let db = RawDb::open(&db_path_for(store)).await.unwrap();
    let (events, unreadable) = db.load_google_events().await.unwrap();
    db.close().await;
    assert!(unreadable.is_empty(), "{unreadable:?}");
    events
        .iter()
        .map(|e| e.event["summary"].as_str().unwrap_or("").to_string())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_sync_lists_the_primary_calendar_and_its_events() {
    let fakes = Fakes::start(&BTreeMap::from([(GOOGLE, seed())])).expect("the fakes serve");
    let store = tempfile::tempdir().unwrap();

    let summary = fetch_into(store.path(), None)
        .await
        .expect("the download runs");

    assert_eq!(
        (summary.calendars, summary.events_new),
        (1, 2),
        "{}",
        summary.line()
    );
    assert_eq!(
        summaries(store.path()).await,
        ["Senior staff briefing", "Diplomatic reception"]
            .map(String::from)
            .into()
    );
    let db = RawDb::open(&db_path_for(store.path())).await.unwrap();
    let (events, _) = db.load_google_events().await.unwrap();
    let briefing = events
        .iter()
        .find(|e| e.event["summary"] == "Senior staff briefing")
        .unwrap();
    assert_eq!(briefing.calendar_id, PICARD);
    assert_eq!(
        briefing.event["attendees"][0]["email"],
        "riker@enterprise.example"
    );
    assert_eq!(briefing.event["location"], "Observation lounge");
    db.close().await;
    assert_eq!(fakes.health()["rate_limited"], json!({}));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_sync_asks_with_the_sync_token_and_adds_the_new_event() {
    let fakes = Fakes::start(&BTreeMap::from([(GOOGLE, seed())])).expect("the fakes serve");
    let store = tempfile::tempdir().unwrap();
    fetch_into(store.path(), None)
        .await
        .expect("the first download runs");
    fakes
        .add_records(
            GOOGLE,
            &json!({"events": [
                {"key": "farpoint-arrival", "owner": PICARD, "summary": "Arrival at Farpoint", "start": "2026-03-23T08:00:00-07:00", "end": "2026-03-23T09:00:00-07:00"},
            ]}),
            None,
        )
        .expect("the update lands");

    fakes.mark_phase("second").unwrap();
    let second = fetch_into(store.path(), None)
        .await
        .expect("the second download runs");

    assert_eq!(
        (second.events_new, second.events_updated),
        (1, 0),
        "{}",
        second.line()
    );
    assert!(summaries(store.path())
        .await
        .contains("Arrival at Farpoint"));
    let with_token = fakes
        .requests_in_phase("second")
        .iter()
        .filter(|line| line["url"].as_str().unwrap().contains("syncToken="))
        .count();
    assert_eq!(with_token, 1, "{:?}", fakes.requests_in_phase("second"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_window_keeps_only_the_events_in_its_days() {
    let _fakes = Fakes::start(&BTreeMap::from([(GOOGLE, seed())])).expect("the fakes serve");
    let store = tempfile::tempdir().unwrap();
    let window = Window {
        start: Some(chrono::NaiveDate::from_ymd_opt(2026, 3, 15).unwrap()),
        end: None,
    };

    let summary = fetch_into(store.path(), Some(window))
        .await
        .expect("the download runs");

    assert_eq!(summary.events_new, 1, "{}", summary.line());
    assert_eq!(
        summaries(store.path()).await,
        ["Senior staff briefing"].map(String::from).into()
    );
}
