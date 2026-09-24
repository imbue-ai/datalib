//! The Google Calendar download on fake TNG data, in the shapes Google's
//! API reference gives for `singleEvents=false` — not yet checked
//! against a live account (see `INGEST.md`).

use std::path::Path;

use datalib_etl::http::{HttpRequest, HttpResponse, LatchkeySettings, PLAYBACK_ENV};
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl::synthesize::{json_response, write_fixture};
use datalib_etl_calendar::ingest::google::{self, calendar_list_url, events_url};
use datalib_etl_calendar::ingest::{db_path_for, FetchSummary, RawDb};
use serde_json::{json, Value};

const PRIMARY: &str = "picard@enterprise.test";
const AWAY: &str = "c_awayteam@group.calendar.google.com";

fn fixture(root: &Path, url: &str, resp: HttpResponse) {
    let req = HttpRequest::get(google::HTTP_SERVICE, url).header("Accept", "application/json");
    write_fixture(root, &req, &resp).expect("write fixture");
}

fn page(items: Value, next_page: Option<&str>, next_sync: Option<&str>) -> HttpResponse {
    let mut v = json!({"kind": "calendar#events", "items": items});
    if let Some(p) = next_page {
        v["nextPageToken"] = json!(p);
    }
    if let Some(s) = next_sync {
        v["nextSyncToken"] = json!(s);
    }
    json_response(&v)
}

fn calendar_list(root: &Path) {
    fixture(
        root,
        &calendar_list_url(None),
        json_response(&json!({
            "kind": "calendar#calendarList",
            "items": [
                {"id": PRIMARY, "summary": PRIMARY, "primary": true, "timeZone": "America/Los_Angeles", "accessRole": "owner"},
                {"id": AWAY, "summary": "Away team", "timeZone": "America/Los_Angeles", "accessRole": "reader"}
            ]
        })),
    );
}

async fn run(playback: &Path, store: &Path) -> FetchSummary {
    std::env::set_var(PLAYBACK_ENV, playback);
    let db = RawDb::open(&db_path_for(store)).await.expect("open store");
    let summary = google::fetch(google::FetchOptions {
        db: db.clone(),
        calendars: Vec::new(),
        latchkey: LatchkeySettings::default(),
        progress: Default::default(),
        control: Default::default(),
    })
    .await;
    db.commit_all("test").await.expect("commit");
    db.close().await;
    std::env::remove_var(PLAYBACK_ENV);
    summary.expect("google fetch under playback")
}

async fn ids(store: &Path) -> Vec<String> {
    let db = RawDb::open(&db_path_for(store))
        .await
        .expect("reopen store");
    let v: Vec<String> = sqlx::query_scalar("SELECT id FROM google_events ORDER BY id")
        .fetch_all(db.pool())
        .await
        .expect("query");
    db.close().await;
    v
}

/// A paged first listing stores the series, its moved and cancelled
/// occurrences and a one-off; the next sync's cancelled series takes its
/// occurrences with it; an expired token on the other calendar is
/// listed whole rather than failing the run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pages_then_syncs_and_survives_an_expired_token() {
    let d = tempfile::tempdir().expect("tempdir");
    let (one, two, store) = (
        d.path().join("one"),
        d.path().join("two"),
        d.path().join("store"),
    );
    std::fs::create_dir_all(&store).unwrap();
    calendar_list(&one);
    calendar_list(&two);

    let away_all = page(
        json!([{
            "id": "away01", "status": "confirmed", "summary": "Away mission: Rigel VII",
            "start": {"dateTime": "2026-10-01T08:00:00-07:00"}, "end": {"dateTime": "2026-10-01T18:00:00-07:00"}
        }]),
        None,
        Some("a1"),
    );
    fixture(
        &one,
        &events_url(PRIMARY, None, None),
        page(
            json!([
                {"id": "staff01", "status": "confirmed", "summary": "Senior staff briefing",
                 "start": {"dateTime": "2026-01-05T09:00:00-08:00", "timeZone": "America/Los_Angeles"},
                 "end": {"dateTime": "2026-01-05T10:00:00-08:00", "timeZone": "America/Los_Angeles"},
                 "recurrence": ["RRULE:FREQ=WEEKLY;BYDAY=MO,TH"]},
                {"id": "staff01_20260312T170000Z", "status": "confirmed", "recurringEventId": "staff01",
                 "summary": "Senior staff briefing — Borg incursion",
                 "originalStartTime": {"dateTime": "2026-03-12T09:00:00-08:00", "timeZone": "America/Los_Angeles"},
                 "start": {"dateTime": "2026-03-12T11:00:00-07:00"}, "end": {"dateTime": "2026-03-12T12:30:00-07:00"}}
            ]),
            Some("p2"),
            None,
        ),
    );
    fixture(
        &one,
        &events_url(PRIMARY, None, Some("p2")),
        page(
            json!([
                {"id": "staff01_20260402T160000Z", "status": "cancelled", "recurringEventId": "staff01",
                 "originalStartTime": {"dateTime": "2026-04-02T09:00:00-07:00", "timeZone": "America/Los_Angeles"}},
                {"id": "reception01", "status": "confirmed", "summary": "Reception for the Klingon delegation",
                 "start": {"dateTime": "2026-09-18T19:00:00-07:00"}, "end": {"dateTime": "2026-09-18T22:00:00-07:00"}},
                {"id": "gone01", "status": "cancelled"}
            ]),
            None,
            Some("s1"),
        ),
    );
    fixture(&one, &events_url(AWAY, None, None), away_all.clone());

    fixture(
        &two,
        &events_url(PRIMARY, Some("s1"), None),
        page(
            json!([{"id": "staff01", "status": "cancelled"}]),
            None,
            Some("s2"),
        ),
    );
    let mut gone = json_response(
        &json!({"error": {"code": 410, "message": "Sync token is no longer valid, a full sync is required."}}),
    );
    gone.status = 410;
    fixture(&two, &events_url(AWAY, Some("a1"), None), gone);
    fixture(&two, &events_url(AWAY, None, None), away_all);

    let first = run(&one, &store).await;
    assert_eq!(
        (first.calendars, first.events_new, first.errors),
        (2, 5, 0),
        "{first:?}"
    );
    assert_eq!(
        ids(&store).await,
        vec![
            format!("{AWAY}#away01"),
            format!("{PRIMARY}#reception01"),
            format!("{PRIMARY}#staff01"),
            format!("{PRIMARY}#staff01_20260312T170000Z"),
            format!("{PRIMARY}#staff01_20260402T160000Z"),
        ],
        "a cancelled one-off is not stored; a cancelled occurrence is"
    );

    let second = run(&two, &store).await;
    assert_eq!((second.events_deleted, second.errors), (3, 0), "{second:?}");
    assert_eq!(
        ids(&store).await,
        vec![format!("{AWAY}#away01"), format!("{PRIMARY}#reception01")],
    );
}
