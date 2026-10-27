//! The Calendly download on fake TNG data. Every reply follows the
//! shapes in Calendly's published API reference (see `INGEST.md`);
//! none was checked against a live account.

use std::path::Path;

use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_calendar::ingest::calendly::{self, events_url, invitees_url, me_url};
use datalib_etl_calendar::ingest::{db_path_for, FetchSummary, RawDb, Window};
use datalib_etl_web::http::{HttpResponse, LatchkeySettings};
use datalib_etl_web::playback;
use datalib_etl_web::synthesize::{json_response, write_fixture};
use datalib_probe::ProbeAsk;
use serde_json::{json, Value};

pub(crate) const USER: &str = "https://api.calendly.com/users/PICARD0000000001";
pub(crate) const CALENDAR: &str = "PICARD0000000001";

pub(crate) fn fixture(root: &Path, url: &str, resp: HttpResponse) {
    write_fixture(root, &calendly::request(url), &resp).expect("write fixture");
}

pub(crate) fn me(root: &Path) {
    fixture(
        root,
        &me_url(),
        json_response(&json!({"resource": {
            "uri": USER, "name": "Jean-Luc Picard", "slug": "picard",
            "email": "picard@enterprise.test",
            "scheduling_url": "https://calendly.example/picard",
            "timezone": "America/Los_Angeles", "time_notation": "24h", "avatar_url": null,
            "created_at": "2024-01-01T00:00:00.000000Z", "updated_at": "2026-01-01T00:00:00.000000Z",
            "current_organization": "https://api.calendly.com/organizations/ENTERPRISE000001",
            "resource_type": "User", "locale": "en"
        }})),
    );
}

/// One page of a listing in Calendly's envelope.
pub(crate) fn page(items: Value, next: Option<&str>) -> HttpResponse {
    json_response(&json!({
        "collection": items,
        "pagination": {
            "count": items.as_array().map_or(0, Vec::len),
            "next_page": next.map(|t| format!("https://api.calendly.com/next?page_token={t}")),
            "previous_page": null,
            "next_page_token": next,
            "previous_page_token": null
        }
    }))
}

/// A scheduled event as the reference describes one.
pub(crate) fn event(uuid: &str, name: &str, start: &str, status: &str) -> Value {
    json!({
        "uri": format!("https://api.calendly.com/scheduled_events/{uuid}"),
        "name": name,
        "meeting_notes_plain": null,
        "meeting_notes_html": null,
        "status": status,
        "start_time": start,
        "end_time": start.replace("T17:00", "T17:30"),
        "event_type": "https://api.calendly.com/event_types/COUNSELING000001",
        "location": {"type": "physical", "location": "Ten Forward", "additional_info": ""},
        "invitees_counter": {"total": 1, "active": 1, "limit": 1},
        "created_at": "2026-10-01T09:00:00.000000Z",
        "updated_at": "2026-10-01T09:00:00.000000Z",
        "event_memberships": [{"user": USER, "user_email": "picard@enterprise.test",
                               "user_name": "Jean-Luc Picard"}],
        "event_guests": [],
        "calendar_event": null
    })
}

pub(crate) fn invitee(event_uuid: &str, uuid: &str, name: &str, email: &str) -> Value {
    json!({
        "uri": format!("https://api.calendly.com/scheduled_events/{event_uuid}/invitees/{uuid}"),
        "email": email, "first_name": null, "last_name": null, "name": name,
        "status": "active",
        "questions_and_answers": [
            {"question": "What would you like to discuss?", "answer": "Shore leave.", "position": 0}
        ],
        "timezone": "America/Los_Angeles",
        "event": format!("https://api.calendly.com/scheduled_events/{event_uuid}"),
        "created_at": "2026-10-01T09:00:00.000000Z", "updated_at": "2026-10-01T09:00:00.000000Z",
        "tracking": {"utm_campaign": null, "utm_source": null, "utm_medium": null,
                     "utm_content": null, "utm_term": null, "salesforce_uuid": null},
        "text_reminder_number": null, "rescheduled": false,
        "old_invitee": null, "new_invitee": null,
        "cancel_url": format!("https://calendly.example/cancellations/{uuid}"),
        "reschedule_url": format!("https://calendly.example/reschedulings/{uuid}"),
        "routing_form_submission": null, "payment": null, "no_show": null,
        "reconfirmation": null, "scheduling_method": null, "invitee_scheduled_by": null
    })
}

pub(crate) async fn run(playback: &Path, store: &Path, window: Option<Window>) -> FetchSummary {
    let db = RawDb::open(&db_path_for(store)).await.expect("open store");
    let download = calendly::fetch(calendly::FetchOptions {
        db: db.clone(),
        window,
        latchkey: LatchkeySettings::default(),
        progress: Default::default(),
        control: Default::default(),
        sealer: None,
    });
    let summary = playback::scope(playback, download)
        .await
        .expect("calendly fetch under playback");
    db.commit_all("test").await.expect("commit");
    db.close().await;
    summary
}

async fn strings(store: &Path, sql: &'static str) -> Vec<String> {
    let db = RawDb::open(&db_path_for(store)).await.unwrap();
    let v = sqlx::query_scalar(sql).fetch_all(db.pool()).await.unwrap();
    db.close().await;
    v
}

async fn event_ids(store: &Path) -> Vec<String> {
    strings(store, "SELECT id FROM calendly_events ORDER BY id").await
}

async fn invitee_ids(store: &Path) -> Vec<String> {
    strings(
        store,
        "SELECT id FROM calendly_invitees WHERE payload IS NOT NULL ORDER BY id",
    )
    .await
}

async fn problem_keys(store: &Path) -> Vec<String> {
    strings(store, "SELECT scope_key FROM problems ORDER BY scope_key").await
}

fn key(uuid: &str) -> String {
    format!("{CALENDAR}#{uuid}")
}

/// A paged listing stores every meeting, canceled ones too, and each
/// meeting's invitees over as many pages as they take. The next run
/// reads the invitees only of the meetings whose listing entry moved,
/// and drops the meeting the listing no longer names with its invitees.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lists_meetings_reads_their_invitees_and_drops_what_is_gone() {
    let d = tempfile::tempdir().expect("tempdir");
    let (one, two, store) = (
        d.path().join("one"),
        d.path().join("two"),
        d.path().join("store"),
    );
    std::fs::create_dir_all(&store).unwrap();
    let counseling = event(
        "EVCOUNSEL0000001",
        "Counseling session",
        "2026-11-02T17:00:00.000000Z",
        "active",
    );
    let poker = event(
        "EVPOKER00000001",
        "Officers' poker night",
        "2026-11-05T17:00:00.000000Z",
        "active",
    );
    let recital = event(
        "EVRECITAL000001",
        "Data's violin recital",
        "2026-11-09T17:00:00.000000Z",
        "canceled",
    );
    for root in [&one, &two] {
        me(root);
    }
    fixture(
        &one,
        &events_url(USER, None, None),
        page(json!([counseling, poker]), Some("p2")),
    );
    fixture(
        &one,
        &events_url(USER, None, Some("p2")),
        page(json!([recital]), None),
    );
    fixture(
        &one,
        &invitees_url("EVCOUNSEL0000001", None),
        page(
            json!([invitee(
                "EVCOUNSEL0000001",
                "INBARCLAY000001",
                "Reginald Barclay",
                "barclay@enterprise.test"
            )]),
            Some("i2"),
        ),
    );
    fixture(
        &one,
        &invitees_url("EVCOUNSEL0000001", Some("i2")),
        page(
            json!([invitee(
                "EVCOUNSEL0000001",
                "INTROI00000001",
                "Deanna Troi",
                "troi@enterprise.test"
            )]),
            None,
        ),
    );
    fixture(
        &one,
        &invitees_url("EVPOKER00000001", None),
        page(
            json!([invitee(
                "EVPOKER00000001",
                "INRIKER0000001",
                "William Riker",
                "riker@enterprise.test"
            )]),
            None,
        ),
    );
    let mut canceled = invitee(
        "EVRECITAL000001",
        "INDATA00000001",
        "Data",
        "data@enterprise.test",
    );
    canceled["status"] = json!("canceled");
    fixture(
        &one,
        &invitees_url("EVRECITAL000001", None),
        page(json!([canceled]), None),
    );

    // The poker night is renamed and the recital is gone; a new meeting
    // is booked. Only those two have invitees to read.
    let mut renamed = poker.clone();
    renamed["name"] = json!("Officers' poker night (high stakes)");
    let briefing = event(
        "EVBRIEF00000001",
        "Away team briefing",
        "2026-11-12T17:00:00.000000Z",
        "active",
    );
    fixture(
        &two,
        &events_url(USER, None, None),
        page(json!([counseling, renamed, briefing]), None),
    );
    fixture(
        &two,
        &invitees_url("EVPOKER00000001", None),
        page(
            json!([invitee(
                "EVPOKER00000001",
                "INRIKER0000001",
                "William Riker",
                "riker@enterprise.test"
            )]),
            None,
        ),
    );
    fixture(
        &two,
        &invitees_url("EVBRIEF00000001", None),
        page(
            json!([invitee(
                "EVBRIEF00000001",
                "INWORF00000001",
                "Worf",
                "worf@enterprise.test"
            )]),
            None,
        ),
    );

    let config: datalib_etl_calendar_config::CalendarConfig =
        serde_json::from_value(json!({"calendly": {}})).unwrap();
    let report = playback::scope(
        &one,
        datalib_etl_calendar::probe::probe(&config, ProbeAsk::Account),
    )
    .await
    .expect("probe under playback");
    assert_eq!(report.mode, "calendly");
    assert_eq!(
        report.account.address.as_deref(),
        Some("picard@enterprise.test")
    );
    assert_eq!(
        report.account.display_name.as_deref(),
        Some("Jean-Luc Picard")
    );

    let first = run(&one, &store, None).await;
    assert_eq!(
        (first.events_new, first.errors, first.requests),
        (3, 0, 7),
        "me, two pages of meetings, four of invitees: {first:?}"
    );
    let all = vec![
        key("EVCOUNSEL0000001"),
        key("EVPOKER00000001"),
        key("EVRECITAL000001"),
    ];
    assert_eq!(event_ids(&store).await, all);
    assert_eq!(invitee_ids(&store).await, all);
    let counsel: Vec<String> = strings(
        &store,
        "SELECT json_extract(value, '$.name') FROM calendly_invitees, \
         json_each(json_extract(payload, '$.invitees')) WHERE calendly_invitees.id LIKE '%EVCOUNSEL%'",
    )
    .await;
    assert_eq!(counsel, vec!["Reginald Barclay", "Deanna Troi"]);

    let second = run(&two, &store, None).await;
    assert_eq!(
        (
            second.events_new,
            second.events_updated,
            second.events_deleted,
            second.errors,
            second.requests
        ),
        (1, 1, 1, 0, 4),
        "me, one page of meetings, the invitees of the two that moved: {second:?}"
    );
    let now = vec![
        key("EVBRIEF00000001"),
        key("EVCOUNSEL0000001"),
        key("EVPOKER00000001"),
    ];
    assert_eq!(event_ids(&store).await, now);
    assert_eq!(invitee_ids(&store).await, now);
    let sidecars = strings(
        &store,
        "SELECT id FROM calendly_invitees_bookkeeping ORDER BY id",
    )
    .await;
    assert_eq!(sidecars, now, "the gone meeting's sidecar went with it");
    assert!(problem_keys(&store).await.is_empty());
}

/// A meeting's invitees that would not come stay owed, with the failure
/// on the record, and the next run asks again; the others are held.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn invitees_that_did_not_come_are_asked_for_again() {
    let d = tempfile::tempdir().expect("tempdir");
    let (one, two, store) = (
        d.path().join("one"),
        d.path().join("two"),
        d.path().join("store"),
    );
    std::fs::create_dir_all(&store).unwrap();
    let counseling = event(
        "EVCOUNSEL0000001",
        "Counseling session",
        "2026-11-02T17:00:00.000000Z",
        "active",
    );
    let poker = event(
        "EVPOKER00000001",
        "Officers' poker night",
        "2026-11-05T17:00:00.000000Z",
        "active",
    );
    for root in [&one, &two] {
        me(root);
        fixture(
            root,
            &events_url(USER, None, None),
            page(json!([counseling, poker]), None),
        );
    }
    fixture(
        &one,
        &invitees_url("EVCOUNSEL0000001", None),
        page(json!([]), None),
    );
    let mut down = json_response(&json!({"title": "Internal Server Error",
        "message": "The server encountered an unexpected condition"}));
    down.status = 500;
    fixture(&one, &invitees_url("EVPOKER00000001", None), down);
    fixture(
        &two,
        &invitees_url("EVPOKER00000001", None),
        page(
            json!([invitee(
                "EVPOKER00000001",
                "INRIKER0000001",
                "William Riker",
                "riker@enterprise.test"
            )]),
            None,
        ),
    );

    let first = run(&one, &store, None).await;
    assert_eq!(first.errors, 1, "{first:?}");
    assert_eq!(invitee_ids(&store).await, vec![key("EVCOUNSEL0000001")]);
    let problems = problem_keys(&store).await;
    assert!(
        problems.len() == 1 && problems[0].contains(&key("EVPOKER00000001")),
        "the failure is on the meeting's record: {problems:?}"
    );

    let second = run(&two, &store, None).await;
    assert_eq!((second.errors, second.requests), (0, 3), "{second:?}");
    assert_eq!(
        invitee_ids(&store).await,
        vec![key("EVCOUNSEL0000001"), key("EVPOKER00000001")]
    );
    assert!(problem_keys(&store).await.is_empty());
}

/// A listing that could not be read whole drops nothing: a page that
/// failed, a reply with no `collection`, a meeting with no `uri`. The
/// next whole listing drops what it does not name, as before.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_listing_it_cannot_fully_read_drops_nothing() {
    let d = tempfile::tempdir().expect("tempdir");
    let roots: Vec<_> = (1..=4).map(|n| d.path().join(format!("run{n}"))).collect();
    let store = d.path().join("store");
    std::fs::create_dir_all(&store).unwrap();
    let window = Window {
        start: chrono::NaiveDate::from_ymd_opt(2026, 11, 1),
        end: chrono::NaiveDate::from_ymd_opt(2026, 12, 1),
    };
    let url = events_url(USER, Some(&window), None);
    assert!(
        url.contains("min_start_time=2026-11-01T00%3A00%3A00Z"),
        "{url}"
    );
    assert!(
        url.contains("max_start_time=2026-12-01T00%3A00%3A00Z"),
        "{url}"
    );
    let counseling = event(
        "EVCOUNSEL0000001",
        "Counseling session",
        "2026-11-02T17:00:00.000000Z",
        "active",
    );
    let poker = event(
        "EVPOKER00000001",
        "Officers' poker night",
        "2026-11-05T17:00:00.000000Z",
        "active",
    );
    let mut nameless = poker.clone();
    nameless.as_object_mut().unwrap().remove("uri");
    for root in &roots {
        me(root);
        for uuid in ["EVCOUNSEL0000001", "EVPOKER00000001"] {
            fixture(root, &invitees_url(uuid, None), page(json!([]), None));
        }
    }
    fixture(
        &roots[0],
        &url,
        page(json!([counseling.clone(), poker]), None),
    );
    // Two pages, the second of which never answers.
    fixture(
        &roots[1],
        &url,
        page(json!([counseling.clone()]), Some("p2")),
    );
    fixture(
        &roots[2],
        &url,
        json_response(&json!({"pagination": {"next_page_token": null}})),
    );
    fixture(
        &roots[3],
        &url,
        page(json!([counseling.clone(), nameless]), None),
    );

    run(&roots[0], &store, Some(window)).await;
    let both = vec![key("EVCOUNSEL0000001"), key("EVPOKER00000001")];
    assert_eq!(event_ids(&store).await, both);
    for root in &roots[1..] {
        let s = run(root, &store, Some(window)).await;
        assert_eq!(s.events_deleted, 0, "{s:?}");
        assert_eq!(event_ids(&store).await, both);
        assert_eq!(problem_keys(&store).await, vec!["listing:meetings"]);
    }

    let fifth = d.path().join("run5");
    me(&fifth);
    fixture(&fifth, &url, page(json!([counseling]), None));
    let s = run(&fifth, &store, Some(window)).await;
    assert_eq!(s.events_deleted, 1, "{s:?}");
    assert_eq!(event_ids(&store).await, vec![key("EVCOUNSEL0000001")]);
    assert!(problem_keys(&store).await.is_empty());
}
