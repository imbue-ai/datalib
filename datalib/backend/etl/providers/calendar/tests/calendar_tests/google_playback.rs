//! The Google Calendar download on fake TNG data. The reply envelope is
//! the one a live account returns; the events inside follow Google's API
//! reference for `singleEvents=false` (see `INGEST.md`).

use datalib_probe::{ProbeAsk, ProbeList};
use std::path::Path;

use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_calendar::ingest::google::{
    self, calendar_list_url, events_url, windowed_events_url,
};
use datalib_etl_calendar::ingest::{db_path_for, FetchSummary, RawDb, Window};
use datalib_etl_web::http::{HttpRequest, HttpResponse, LatchkeySettings, PLAYBACK_ENV};
use datalib_etl_web::synthesize::{json_response, write_fixture};
use serde_json::{json, Value};

pub(crate) const PRIMARY: &str = "picard@enterprise.test";
pub(crate) const AWAY: &str = "c_awayteam@group.calendar.google.com";

pub(crate) fn fixture(root: &Path, url: &str, resp: HttpResponse) {
    let req = HttpRequest::get(google::HTTP_SERVICE, url).header("Accept", "application/json");
    write_fixture(root, &req, &resp).expect("write fixture");
}

pub(crate) fn page(items: Value, next_page: Option<&str>, next_sync: Option<&str>) -> HttpResponse {
    let mut v = json!({"kind": "calendar#events", "items": items});
    if let Some(p) = next_page {
        v["nextPageToken"] = json!(p);
    }
    if let Some(s) = next_sync {
        v["nextSyncToken"] = json!(s);
    }
    json_response(&v)
}

pub(crate) fn calendar_list(root: &Path) {
    calendar_list_of(root, true);
}

/// The account's calendars, with or without the away team's.
fn calendar_list_of(root: &Path, with_away: bool) {
    let mut items = vec![
        json!({"id": PRIMARY, "summary": PRIMARY, "primary": true, "timeZone": "America/Los_Angeles", "accessRole": "owner"}),
    ];
    if with_away {
        items.push(json!({"id": AWAY, "summary": "Away team", "timeZone": "America/Los_Angeles", "accessRole": "reader"}));
    }
    fixture(
        root,
        &calendar_list_url(None),
        json_response(&json!({"kind": "calendar#calendarList", "items": items})),
    );
}

async fn run(playback: &Path, store: &Path) -> FetchSummary {
    run_in(playback, store, None).await
}

async fn run_in(playback: &Path, store: &Path, window: Option<Window>) -> FetchSummary {
    std::env::set_var(PLAYBACK_ENV, playback);
    let db = RawDb::open(&db_path_for(store)).await.expect("open store");
    let summary = google::fetch(google::FetchOptions {
        db: db.clone(),
        calendars: Vec::new(),
        window,
        latchkey: LatchkeySettings::default(),
        progress: Default::default(),
        control: Default::default(),
        sealer: None,
    })
    .await;
    if summary.is_ok() {
        db.commit_all("test").await.expect("commit");
    }
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
                {"id": "gone01", "status": "cancelled"},
                // A deleted series lists as cancelled, and its occurrences
                // may come on either side of it; neither side is stored.
                {"id": "old01_20260105T170000Z", "status": "cancelled", "recurringEventId": "old01",
                 "originalStartTime": {"dateTime": "2026-01-05T09:00:00-08:00"}},
                {"id": "old01", "status": "cancelled", "recurrence": ["RRULE:FREQ=DAILY"]},
                {"id": "old01_20260106T170000Z", "status": "confirmed", "recurringEventId": "old01",
                 "originalStartTime": {"dateTime": "2026-01-06T09:00:00-08:00"},
                 "start": {"dateTime": "2026-01-06T10:00:00-08:00"}}
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

    std::env::set_var(PLAYBACK_ENV, &one);
    let config: datalib_etl_calendar_config::CalendarConfig =
        serde_json::from_value(json!({"google": {}})).unwrap();
    let report =
        datalib_etl_calendar::probe::probe(&config, ProbeAsk::List(ProbeList::Calendars)).await;
    std::env::remove_var(PLAYBACK_ENV);
    let report = report.expect("probe under playback");
    assert_eq!(report.account.address.as_deref(), Some(PRIMARY));
    let items: Vec<(&str, Option<&str>)> = report
        .items
        .iter()
        .map(|i| (i.path.as_str(), i.role.as_deref()))
        .collect();
    assert_eq!(
        items,
        vec![("Away team", Some("read-only")), (PRIMARY, Some("primary"))]
    );

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
        "a cancelled one-off is not stored, nor a deleted series' occurrences; \
         a cancelled occurrence of a live series is"
    );

    let second = run(&two, &store).await;
    assert_eq!((second.events_deleted, second.errors), (3, 0), "{second:?}");
    assert_eq!(
        ids(&store).await,
        vec![format!("{AWAY}#away01"), format!("{PRIMARY}#reception01")],
    );
}

/// A windowed calendar is listed whole every run with the window's
/// bounds and no sync token, and what drops out of the window is dropped
/// from the mirror — the listing is the window as it is now.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_window_is_listed_whole_every_run_and_keeps_no_token() {
    let d = tempfile::tempdir().expect("tempdir");
    let (one, two, store) = (
        d.path().join("one"),
        d.path().join("two"),
        d.path().join("store"),
    );
    std::fs::create_dir_all(&store).unwrap();
    let window = Window {
        start: chrono::NaiveDate::from_ymd_opt(2026, 3, 1),
        end: chrono::NaiveDate::from_ymd_opt(2026, 4, 1),
    };
    let url = windowed_events_url(PRIMARY, &window, None);
    assert!(url.contains("timeMin=2026-03-01T00%3A00%3A00Z"), "{url}");
    assert!(url.contains("timeMax=2026-04-01T00%3A00%3A00Z"), "{url}");
    assert!(!url.contains("syncToken"), "{url}");

    let series = json!({"id": "staff01", "status": "confirmed", "summary": "Senior staff briefing",
        "start": {"dateTime": "2026-01-05T09:00:00-08:00"}, "end": {"dateTime": "2026-01-05T10:00:00-08:00"},
        "recurrence": ["RRULE:FREQ=WEEKLY;BYDAY=MO,TH"]});
    let moved = json!({"id": "staff01_20260312T170000Z", "status": "confirmed", "recurringEventId": "staff01",
        "originalStartTime": {"dateTime": "2026-03-12T09:00:00-08:00"},
        "start": {"dateTime": "2026-03-12T11:00:00-07:00"}, "end": {"dateTime": "2026-03-12T12:30:00-07:00"}});
    for root in [&one, &two] {
        calendar_list(root);
        fixture(
            root,
            &windowed_events_url(AWAY, &window, None),
            page(json!([]), None, Some("a1")),
        );
    }
    // Google hands back a sync token even here; it must not be kept.
    fixture(
        &one,
        &url,
        page(json!([series.clone(), moved]), None, Some("s1")),
    );
    fixture(&two, &url, page(json!([series]), None, Some("s2")));

    let first = run_in(&one, &store, Some(window)).await;
    assert_eq!((first.events_new, first.errors), (2, 0), "{first:?}");
    let second = run_in(&two, &store, Some(window)).await;
    assert_eq!((second.events_deleted, second.errors), (1, 0), "{second:?}");
    assert_eq!(ids(&store).await, vec![format!("{PRIMARY}#staff01")]);

    let db = RawDb::open(&db_path_for(&store)).await.unwrap();
    let tokens: Vec<Option<String>> = sqlx::query_scalar("SELECT sync_token FROM calendars")
        .fetch_all(db.pool())
        .await
        .unwrap();
    db.close().await;
    assert!(tokens.iter().all(Option::is_none), "{tokens:?}");
}

/// A calendar that would not list is a row, and a run that stopped
/// before it reached any calendar leaves that row standing: it listed
/// nothing, so it cannot say the calendar answers again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_run_leaves_the_last_listing_rows() {
    let d = tempfile::tempdir().expect("tempdir");
    let (playback, store) = (d.path().join("playback"), d.path().join("store"));
    std::fs::create_dir_all(&store).unwrap();
    calendar_list(&playback);
    fixture(
        &playback,
        &events_url(PRIMARY, None, None),
        page(json!([]), None, Some("p1")),
    );
    // No fixture for the away team's events: that listing fails.
    run(&playback, &store).await;
    let failed = vec!["listing:calendar Away team".to_string()];
    assert_eq!(problem_keys(&store).await, failed);

    std::env::set_var(PLAYBACK_ENV, &playback);
    let db = RawDb::open(&db_path_for(&store)).await.expect("open store");
    let control = datalib_etl::control::DownloadControl::default();
    control.stop.request();
    google::fetch(google::FetchOptions {
        db: db.clone(),
        calendars: Vec::new(),
        window: None,
        latchkey: LatchkeySettings::default(),
        progress: Default::default(),
        control,
        sealer: None,
    })
    .await
    .expect("a stopped run");
    db.commit_all("test").await.expect("commit");
    db.close().await;
    std::env::remove_var(PLAYBACK_ENV);
    assert_eq!(problem_keys(&store).await, failed);
}

async fn problem_keys(store: &Path) -> Vec<String> {
    let db = RawDb::open(&db_path_for(store)).await.unwrap();
    let keys = sqlx::query_scalar("SELECT scope_key FROM problems ORDER BY scope_key")
        .fetch_all(db.pool())
        .await
        .unwrap();
    db.close().await;
    keys
}

/// A whole listing may delete what it does not name only if it named
/// everything it listed. An event it could not identify (no `id`) used to
/// be skipped and the stored copy deleted, and a reply with no `items`
/// read as an empty calendar (audit 2026-10-02 §4). Google sends `items`
/// on every reply, `[]` when empty — measured live across 7 calendars,
/// incremental replies included — so its absence is not a listing.
/// Either now deletes nothing and says why on the calendar's row; the
/// next listing that names everything deletes as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_whole_listing_it_cannot_fully_read_deletes_nothing() {
    let d = tempfile::tempdir().expect("tempdir");
    let (one, two, three, four, store) = (
        d.path().join("one"),
        d.path().join("two"),
        d.path().join("three"),
        d.path().join("four"),
        d.path().join("store"),
    );
    std::fs::create_dir_all(&store).unwrap();
    let window = Window {
        start: chrono::NaiveDate::from_ymd_opt(2026, 3, 1),
        end: chrono::NaiveDate::from_ymd_opt(2026, 4, 1),
    };
    let url = windowed_events_url(PRIMARY, &window, None);
    let series = json!({"id": "staff01", "status": "confirmed", "summary": "Senior staff briefing",
        "start": {"dateTime": "2026-01-05T09:00:00-08:00"}, "end": {"dateTime": "2026-01-05T10:00:00-08:00"},
        "recurrence": ["RRULE:FREQ=WEEKLY;BYDAY=MO,TH"]});
    let moved = json!({"id": "staff01_20260312T170000Z", "status": "confirmed", "recurringEventId": "staff01",
        "originalStartTime": {"dateTime": "2026-03-12T09:00:00-08:00"},
        "start": {"dateTime": "2026-03-12T11:00:00-07:00"}, "end": {"dateTime": "2026-03-12T12:30:00-07:00"}});
    let mut moved_without_id = moved.clone();
    moved_without_id.as_object_mut().unwrap().remove("id");
    for root in [&one, &two, &three, &four] {
        calendar_list(root);
        fixture(
            root,
            &windowed_events_url(AWAY, &window, None),
            page(json!([]), None, Some("a1")),
        );
    }
    fixture(&one, &url, page(json!([series.clone(), moved]), None, None));
    fixture(
        &two,
        &url,
        page(json!([series.clone(), moved_without_id]), None, None),
    );
    fixture(&three, &url, page(json!([series]), None, None));
    fixture(
        &four,
        &url,
        json_response(&json!({"kind": "calendar#events", "nextSyncToken": "s4"})),
    );

    run_in(&one, &store, Some(window)).await;
    let stored = ids(&store).await;
    assert_eq!(stored.len(), 2, "{stored:?}");
    let listing = format!("listing:calendar {PRIMARY}");

    let second = run_in(&two, &store, Some(window)).await;
    assert_eq!(second.events_deleted, 0, "{second:?}");
    assert_eq!(ids(&store).await, stored);
    assert_eq!(problem_keys(&store).await, vec![listing.clone()]);

    let third = run_in(&three, &store, Some(window)).await;
    assert_eq!(third.events_deleted, 1, "{third:?}");
    assert_eq!(ids(&store).await, vec![format!("{PRIMARY}#staff01")]);
    assert_eq!(problem_keys(&store).await, Vec::<String>::new());

    let fourth = run_in(&four, &store, Some(window)).await;
    assert_eq!(fourth.events_deleted, 0, "{fourth:?}");
    assert_eq!(ids(&store).await, vec![format!("{PRIMARY}#staff01")]);
    assert_eq!(problem_keys(&store).await, vec![listing]);
}

/// A calendar the account's list no longer names goes with its events:
/// the list is whole by nature, so absence from it is deletion.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_calendar_the_list_no_longer_names_goes_with_its_events() {
    let d = tempfile::tempdir().expect("tempdir");
    let (one, two, store) = (
        d.path().join("one"),
        d.path().join("two"),
        d.path().join("store"),
    );
    std::fs::create_dir_all(&store).unwrap();
    calendar_list(&one);
    let away = json!({"id": "away01", "status": "confirmed", "summary": "Away mission: Rigel VII",
        "start": {"dateTime": "2026-10-01T08:00:00-07:00"}, "end": {"dateTime": "2026-10-01T18:00:00-07:00"}});
    fixture(
        &one,
        &events_url(PRIMARY, None, None),
        page(json!([]), None, Some("s1")),
    );
    fixture(
        &one,
        &events_url(AWAY, None, None),
        page(json!([away]), None, Some("a1")),
    );
    calendar_list_of(&two, false);
    fixture(
        &two,
        &events_url(PRIMARY, Some("s1"), None),
        page(json!([]), None, Some("s1")),
    );

    let first = run(&one, &store).await;
    assert_eq!(first.events_new, 1, "{first:?}");
    let second = run(&two, &store).await;
    assert_eq!(
        (second.calendars, second.events_deleted),
        (1, 1),
        "{second:?}"
    );
    assert!(ids(&store).await.is_empty());
}
