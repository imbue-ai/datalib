use chrono::NaiveDate;
use datalib_etl_calendar::ingest::db::LoadedCalendar;
use serde_json::json;

use super::*;

const PICARD: &str = "picard@enterprise.test";
const LA: &str = "America/Los_Angeles";

fn window(from: &str, until: &str) -> Window {
    let p = |s: &str| {
        NaiveDate::parse_from_str(s, "%Y-%m-%d")
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
    };
    Window {
        from: p(from),
        until: p(until),
    }
}

fn google(events: Vec<Value>) -> Parsed {
    Parsed {
        calendars: vec![LoadedCalendar {
            id: PICARD.into(),
            display_name: None,
            time_zone: Some(LA.into()),
        }],
        google: events
            .into_iter()
            .map(|event| LoadedGoogleEvent {
                id: format!("{PICARD}#{}", event["id"].as_str().unwrap()),
                calendar_id: PICARD.into(),
                event,
            })
            .collect(),
        ..Parsed::default()
    }
}

fn ids(listed: &Listed) -> Vec<&str> {
    listed.rows.iter().map(|r| r.instance_id.as_str()).collect()
}

fn row<'a>(listed: &'a Listed, instance_id: &str) -> &'a OccurrenceRow {
    listed
        .rows
        .iter()
        .find(|r| r.instance_id == instance_id)
        .unwrap_or_else(|| panic!("no {instance_id} in {:?}", ids(listed)))
}

fn at(t: &str) -> Value {
    json!({"dateTime": t, "timeZone": LA})
}

/// "This and following" splits a Google series: the old one gains an
/// `UNTIL`, the new one is `<id>_R<its first start>`, and its
/// occurrences — and the rows that change them — are named `<id>_<the
/// original start in UTC>Z`, without the `_R…`. The weekly 10:00 stays
/// 10:00 when the clocks go back on 1 Nov; a moved occurrence stands in
/// for its date, and a cancelled one and an `EXDATE;TZID=` take theirs
/// away.
#[test]
fn a_split_google_series_lists_like_single_events() {
    let parsed = google(vec![
        json!({
            "id": "staff01", "status": "confirmed", "summary": "Senior staff",
            "start": at("2026-09-07T09:00:00-07:00"), "end": at("2026-09-07T10:00:00-07:00"),
            "recurrence": ["RRULE:FREQ=WEEKLY;BYDAY=MO;UNTIL=20260927T235959Z"]
        }),
        json!({
            "id": "staff01_R20260928T170000", "status": "confirmed", "summary": "Senior staff",
            "updated": "2026-09-20T12:00:00.000Z",
            "start": at("2026-09-28T10:00:00-07:00"), "end": at("2026-09-28T11:00:00-07:00"),
            "recurrence": ["RRULE:FREQ=WEEKLY;BYDAY=MO",
                           "EXDATE;TZID=America/Los_Angeles:20261026T100000"]
        }),
        json!({
            "id": "staff01_20261012T170000Z", "status": "confirmed",
            "recurringEventId": "staff01_R20260928T170000", "summary": "Senior staff — moved",
            "originalStartTime": at("2026-10-12T10:00:00-07:00"),
            "start": at("2026-10-13T14:00:00-07:00"), "end": at("2026-10-13T15:00:00-07:00")
        }),
        json!({
            "id": "staff01_20261019T170000Z", "status": "cancelled",
            "recurringEventId": "staff01_R20260928T170000",
            "originalStartTime": at("2026-10-19T10:00:00-07:00")
        }),
        json!({
            "id": "staff01_20261109T180000Z", "status": "confirmed",
            "recurringEventId": "staff01_R20260928T170000",
            "originalStartTime": at("2026-11-09T10:00:00-08:00"),
            "start": at("2026-11-09T10:00:00-08:00"), "end": at("2026-11-09T11:30:00-08:00")
        }),
    ]);
    let listed = list(&parsed, &window("2026-09-01", "2026-11-10"));
    assert_eq!(listed.unlisted, []);
    assert_eq!(
        ids(&listed),
        [
            "staff01_20260907T160000Z",
            "staff01_20260914T160000Z",
            "staff01_20260921T160000Z",
            "staff01_20260928T170000Z",
            "staff01_20261005T170000Z",
            "staff01_20261012T170000Z",
            "staff01_20261102T180000Z",
            "staff01_20261109T180000Z",
        ]
    );

    let moved = row(&listed, "staff01_20261012T170000Z");
    assert_eq!(moved.series_id.as_deref(), Some("staff01_R20260928T170000"));
    assert_eq!(moved.start_time, "2026-10-13T14:00:00-07:00");
    assert_eq!(
        moved.original_start_time.as_deref(),
        Some("2026-10-12T10:00:00-07:00")
    );
    assert_eq!(moved.title.as_deref(), Some("Senior staff — moved"));
    assert_eq!(
        moved.event_row,
        format!("{PICARD}#staff01_20261012T170000Z")
    );

    let winter = row(&listed, "staff01_20261102T180000Z");
    assert_eq!(winter.start_time, "2026-11-02T10:00:00-08:00");
    assert_eq!(
        winter.end_time.as_deref(),
        Some("2026-11-02T11:00:00-08:00")
    );
    assert_eq!(winter.start_utc, "2026-11-02T18:00:00Z");
    assert_eq!(
        winter.series_id.as_deref(),
        Some("staff01_R20260928T170000")
    );
    assert_eq!(winter.time_zone.as_deref(), Some(LA));
    assert!(!winter.all_day);

    // The payload is the occurrence as `singleEvents=true` returns it,
    // `updated` included.
    let payload: Value = serde_json::from_str(&winter.payload).unwrap();
    assert_eq!(payload["id"], "staff01_20261102T180000Z");
    assert_eq!(payload["recurringEventId"], "staff01_R20260928T170000");
    assert_eq!(payload["start"], at("2026-11-02T10:00:00-08:00"));
    assert_eq!(
        payload["originalStartTime"],
        at("2026-11-02T10:00:00-08:00")
    );
    assert_eq!(payload["updated"], "2026-09-20T12:00:00.000Z");
    assert!(payload.get("recurrence").is_none());

    // Made longer, with no summary of its own: it takes the series'.
    let longer = row(&listed, "staff01_20261109T180000Z");
    assert_eq!(
        longer.end_time.as_deref(),
        Some("2026-11-09T11:30:00-08:00")
    );
    assert_eq!(longer.title.as_deref(), Some("Senior staff"));
}

/// An all-day series runs on dates; Google names each occurrence by
/// its date alone, and the window reads a date as the calendar's day.
#[test]
fn an_all_day_google_series_is_named_by_date() {
    let parsed = google(vec![json!({
        "id": "birthday01", "status": "confirmed", "summary": "Data's birthday",
        "start": {"date": "2026-11-05"}, "end": {"date": "2026-11-06"},
        "recurrence": ["RRULE:FREQ=YEARLY"]
    })]);
    let listed = list(&parsed, &window("2026-01-01", "2028-12-31"));
    assert_eq!(
        ids(&listed),
        [
            "birthday01_20261105",
            "birthday01_20271105",
            "birthday01_20281105"
        ]
    );
    let first = &listed.rows[0];
    assert!(first.all_day);
    assert_eq!(first.start_time, "2026-11-05");
    assert_eq!(first.end_time.as_deref(), Some("2026-11-06"));
    assert_eq!(first.original_start_time.as_deref(), Some("2026-11-05"));
    assert_eq!(first.start_utc, "2026-11-05T08:00:00Z");
    assert_eq!(first.time_zone.as_deref(), Some(LA));
    let payload: Value = serde_json::from_str(&first.payload).unwrap();
    assert_eq!(payload["start"], json!({"date": "2026-11-05"}));
}

/// A moved occurrence is listed where it went: one moved into the
/// window from outside it is in, and the date one moved away from is
/// empty.
#[test]
fn a_moved_occurrence_is_listed_where_it_moved_to() {
    let parsed = google(vec![
        json!({
            "id": "drill01", "status": "confirmed", "summary": "Saucer separation drill",
            "start": at("2026-10-01T15:00:00-07:00"), "end": at("2026-10-01T15:30:00-07:00"),
            "recurrence": ["RRULE:FREQ=DAILY"]
        }),
        json!({
            "id": "drill01_20261020T220000Z", "status": "confirmed",
            "recurringEventId": "drill01",
            "originalStartTime": at("2026-10-20T15:00:00-07:00"),
            "start": at("2026-10-10T08:00:00-07:00"), "end": at("2026-10-10T08:30:00-07:00")
        }),
        json!({
            "id": "drill01_20261010T220000Z", "status": "confirmed",
            "recurringEventId": "drill01",
            "originalStartTime": at("2026-10-10T15:00:00-07:00"),
            "start": at("2026-10-25T15:00:00-07:00"), "end": at("2026-10-25T15:30:00-07:00")
        }),
    ]);
    let listed = list(&parsed, &window("2026-10-10", "2026-10-11"));
    assert_eq!(
        ids(&listed),
        ["drill01_20261020T220000Z"],
        "the 20th's moved in; the 10th's moved out"
    );
    assert_eq!(listed.rows[0].start_utc, "2026-10-10T15:00:00Z");
}

/// An invitation to one occurrence of someone else's series comes as
/// the changed occurrence alone.
#[test]
fn an_invitation_to_one_occurrence_is_listed_alone() {
    let parsed = google(vec![
        json!({
            "id": "lecture01_20261005T210000Z", "status": "confirmed",
            "recurringEventId": "lecture01", "summary": "Guest lecture",
            "originalStartTime": at("2026-10-05T14:00:00-07:00"),
            "start": at("2026-10-05T14:00:00-07:00"), "end": at("2026-10-05T15:00:00-07:00")
        }),
        json!({
            "id": "poker01", "status": "cancelled",
            "start": at("2026-10-06T20:00:00-07:00"), "end": at("2026-10-06T23:00:00-07:00")
        }),
    ]);
    let listed = list(&parsed, &window("2026-10-01", "2026-10-31"));
    assert_eq!(ids(&listed), ["lecture01_20261005T210000Z"]);
    assert_eq!(listed.rows[0].series_id.as_deref(), Some("lecture01"));
}

/// A CalDAV object holds the series and its changed occurrences: an
/// `EXDATE;TZID=`, a moved occurrence that says no end and keeps the
/// series' hour, and one cancelled by an override with `STATUS`.
#[test]
fn a_caldav_series_lists_its_occurrences_with_their_changes() {
    let ics = "BEGIN:VCALENDAR\r\n\
BEGIN:VEVENT\r\nUID:away-team\r\nSUMMARY:Away team\r\n\
DTSTART;TZID=America/New_York:20261026T090000\r\nDTEND;TZID=America/New_York:20261026T100000\r\n\
RRULE:FREQ=WEEKLY;BYDAY=MO;COUNT=4\r\nEXDATE;TZID=America/New_York:20261109T090000\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:away-team\r\nRECURRENCE-ID;TZID=America/New_York:20261102T090000\r\n\
SUMMARY:Away team (moved)\r\nDTSTART;TZID=America/New_York:20261102T110000\r\nEND:VEVENT\r\n\
BEGIN:VEVENT\r\nUID:away-team\r\nRECURRENCE-ID:20261116T140000Z\r\nSTATUS:CANCELLED\r\n\
DTSTART;TZID=America/New_York:20261116T090000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
    let parsed = Parsed {
        calendars: vec![LoadedCalendar {
            id: "bridge".into(),
            display_name: None,
            time_zone: Some(LA.into()),
        }],
        ics: vec![LoadedIcsObject {
            id: "bridge#away-team".into(),
            calendar_id: "bridge".into(),
            uid: "away-team".into(),
            ics: ics.into(),
        }],
        ..Parsed::default()
    };
    let listed = list(&parsed, &window("2026-10-01", "2027-01-01"));
    assert_eq!(listed.unlisted, []);
    assert_eq!(
        ids(&listed),
        ["away-team_20261026T130000Z", "away-team_20261102T140000Z"]
    );
    let first = &listed.rows[0];
    assert_eq!(first.start_time, "2026-10-26T09:00:00-04:00");
    assert_eq!(first.series_id.as_deref(), Some("away-team"));
    assert_eq!(first.title.as_deref(), Some("Away team"));
    assert!(first.payload.contains("RRULE:FREQ=WEEKLY"));
    let moved = &listed.rows[1];
    assert_eq!(moved.start_time, "2026-11-02T11:00:00-05:00");
    assert_eq!(moved.end_time.as_deref(), Some("2026-11-02T12:00:00-05:00"));
    assert_eq!(
        moved.original_start_time.as_deref(),
        Some("2026-11-02T09:00:00-05:00")
    );
    assert_eq!(moved.title.as_deref(), Some("Away team (moved)"));
    assert_eq!(moved.status, "confirmed", "no STATUS reads as confirmed");
}

/// A one-off event is listed under its own id; one whose zone or rule
/// cannot be read is said, not guessed.
#[test]
fn what_cannot_be_listed_is_said() {
    let one = |uid: &str, body: &str| LoadedIcsObject {
        id: format!("bridge#{uid}"),
        calendar_id: "bridge".into(),
        uid: uid.into(),
        ics: format!(
            "BEGIN:VCALENDAR\r\nBEGIN:VEVENT\r\nUID:{uid}\r\n{body}END:VEVENT\r\nEND:VCALENDAR\r\n"
        ),
    };
    let parsed = Parsed {
        ics: vec![
            one(
                "poker",
                "DTSTART:20261006T030000Z\r\nDTEND:20261006T060000Z\r\n",
            ),
            one(
                "outlook",
                "DTSTART;TZID=Pacific Standard Time:20261006T090000\r\n",
            ),
            one(
                "fortnightly",
                "DTSTART:20261006T090000Z\r\nRRULE:FREQ=FORTNIGHTLY\r\n",
            ),
        ],
        ..Parsed::default()
    };
    let listed = list(&parsed, &window("2026-10-01", "2026-11-01"));
    assert_eq!(ids(&listed), ["poker"]);
    assert_eq!(listed.rows[0].series_id, None);
    assert_eq!(listed.rows[0].original_start_time, None);
    let reasons: Vec<(&str, &str)> = listed
        .unlisted
        .iter()
        .map(|u| (u.event_row.as_str(), u.reason.as_str()))
        .collect();
    assert_eq!(
        reasons,
        [
            (
                "bridge#outlook",
                "unknown time zone \"Pacific Standard Time\""
            ),
            (
                "bridge#fortnightly",
                "repeat rule: cannot follow `FREQ=FORTNIGHTLY`"
            ),
        ]
    );
}

#[test]
fn only_a_split_stamp_is_taken_off_a_series_id() {
    let t = EventTime::from_ical("20261022T130000Z", None).unwrap();
    assert_eq!(
        google_instance_id("abc_R20261001T130000", &t),
        "abc_20261022T130000Z"
    );
    assert_eq!(
        google_instance_id("abc_R20261001", &t),
        "abc_20261022T130000Z"
    );
    assert_eq!(google_instance_id("abc_Rx", &t), "abc_Rx_20261022T130000Z");
    assert_eq!(
        google_instance_id("a_R2026", &t),
        "a_R2026_20261022T130000Z"
    );
}

/// The store holds what the last pass listed and nothing older.
#[tokio::test(flavor = "multi_thread")]
async fn the_store_holds_the_last_pass_and_its_window() {
    let td = tempfile::tempdir().unwrap();
    let db = td.path().join("occurrences.doltlite_db");
    let parsed = google(vec![json!({
        "id": "drill01", "status": "confirmed",
        "start": at("2026-10-01T15:00:00-07:00"), "end": at("2026-10-01T15:30:00-07:00"),
        "recurrence": ["RRULE:FREQ=DAILY"]
    })]);
    for (from, until, want) in [
        ("2026-10-01", "2026-10-04", 3),
        ("2026-10-02", "2026-10-04", 2),
    ] {
        let w = window(from, until);
        let pool = store::open(&db).await.unwrap();
        store::replace(&pool, &list(&parsed, &w), &w).await.unwrap();
        doltlite_raw_commit(&pool).await;
        let n: i64 = sqlx::query_scalar("SELECT count(*) FROM occurrences")
            .fetch_one(&pool)
            .await
            .unwrap();
        let from_utc: String = sqlx::query_scalar("SELECT from_utc FROM occurrences_window")
            .fetch_one(&pool)
            .await
            .unwrap();
        pool.close().await;
        assert_eq!(n, want);
        assert_eq!(from_utc, format!("{from}T00:00:00Z"));
    }
}

async fn doltlite_raw_commit(pool: &sqlx::SqlitePool) {
    datalib_etl::doltlite_raw::commit_run(pool, "test")
        .await
        .unwrap();
}

/// Measured live: Google stops a rule that never ends after 730
/// occurrences. A daily series begun in 2020 has none in 2026, while one
/// with its own `COUNT` or `UNTIL`, or on a CalDAV calendar, runs as
/// written.
#[test]
fn google_stops_a_rule_that_never_ends_at_730_occurrences() {
    let daily = |id: &str, rule: &str| {
        json!({
            "id": id, "status": "confirmed",
            "start": at("2020-07-21T08:00:00-07:00"), "end": at("2020-07-21T08:15:00-07:00"),
            "recurrence": [rule]
        })
    };
    let parsed = google(vec![
        daily("open01", "RRULE:FREQ=DAILY"),
        daily("until01", "RRULE:FREQ=DAILY;UNTIL=20261231T000000Z"),
    ]);
    let listed = list(&parsed, &window("2022-07-19", "2022-07-23"));
    let open: Vec<&str> = ids(&listed)
        .into_iter()
        .filter(|i| i.starts_with("open01"))
        .collect();
    assert_eq!(
        open,
        ["open01_20220719T150000Z", "open01_20220720T150000Z"],
        "the 730th is 20 July 2022"
    );
    let later = list(&parsed, &window("2026-10-01", "2026-10-03"));
    assert_eq!(
        ids(&later),
        ["until01_20261001T150000Z", "until01_20261002T150000Z"]
    );
    assert_eq!(
        google_bounded("RRULE:FREQ=WEEKLY;COUNT=3"),
        "RRULE:FREQ=WEEKLY;COUNT=3"
    );
    assert_eq!(google_bounded("FREQ=MONTHLY"), "FREQ=MONTHLY;COUNT=730");
}
