//! A CalDAV account shaped like Fastmail's, measured live on
//! 2026-09-24: the bare host answers 404, `/.well-known/caldav` redirects
//! to the DAV root, every value is CDATA, and the home lists scheduling
//! boxes beside the calendars.

use std::collections::BTreeMap;
use std::path::Path;

use datalib_etl::http::{HttpMethod, HttpResponse, LatchkeySettings, PLAYBACK_ENV};
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl::synthesize::write_fixture;
use datalib_etl_calendar::ingest::caldav::{self, dav};
use datalib_etl_calendar::ingest::{db_path_for, FetchSummary, RawDb};

const HOST: &str = "https://caldav.enterprise.test";
const PRINCIPAL: &str = "/dav/principals/user/picard@enterprise.test/";
const HOME: &str = "/dav/calendars/user/picard@enterprise.test/";
const BRIDGE: &str = "/dav/calendars/user/picard@enterprise.test/2c1f4e0a-bridge/";

const STAFF: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Fastmail/2020.5/EN\r\nBEGIN:VEVENT\r\nUID:tng-staff@enterprise.test\r\nSUMMARY:Senior staff briefing\r\nDTSTART;TZID=America/Los_Angeles:20260105T090000\r\nDTEND;TZID=America/Los_Angeles:20260105T100000\r\nRRULE:FREQ=WEEKLY;BYDAY=MO,TH\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";
const RECEPTION: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nPRODID:-//Fastmail/2020.5/EN\r\nBEGIN:VEVENT\r\nUID:tng-reception@enterprise.test\r\nSUMMARY:Reception for the Klingon delegation\r\nDTSTART;TZID=America/Los_Angeles:20260918T190000\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

fn xml(status: u16, body: &str) -> HttpResponse {
    let mut headers = BTreeMap::new();
    headers.insert(
        "content-type".into(),
        "application/xml; charset=utf-8".into(),
    );
    HttpResponse {
        status,
        headers,
        body: body.as_bytes().to_vec(),
        duration_ms: 0,
    }
}

fn fixture(
    root: &Path,
    method: HttpMethod,
    url: &str,
    depth: &str,
    body: &str,
    resp: HttpResponse,
) {
    let req = dav::http_request(method, url, depth, body, &LatchkeySettings::default());
    write_fixture(root, &req, &resp).expect("write fixture");
}

fn resource(href: &str, etag: &str, ics: &str) -> String {
    format!(
        "<response><href>{href}</href><propstat><prop><getetag><![CDATA[{etag}]]></getetag>\
         <C:calendar-data><![CDATA[{ics}]]></C:calendar-data></prop>\
         <status>HTTP/1.1 200 OK</status></propstat></response>"
    )
}

fn multistatus(inner: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="utf-8"?><multistatus xmlns="DAV:" xmlns:C="urn:ietf:params:xml:ns:caldav" xmlns:A="http://apple.com/ns/ical/">{inner}</multistatus>"#
    )
}

/// Discovery and the calendar listing, shared by both runs.
fn account_fixtures(root: &Path) {
    let find_principal = dav::BODY_CURRENT_USER_PRINCIPAL;
    fixture(
        root,
        HttpMethod::Propfind,
        &format!("{HOST}/"),
        "0",
        find_principal,
        xml(
            404,
            "<html><head><title>404 Not Found</title></head></html>",
        ),
    );
    let mut moved = xml(301, "");
    moved
        .headers
        .insert("location".into(), format!("{HOST}/dav/calendars"));
    fixture(
        root,
        HttpMethod::Propfind,
        &format!("{HOST}/.well-known/caldav"),
        "0",
        find_principal,
        moved,
    );
    fixture(root, HttpMethod::Propfind, &format!("{HOST}/dav/calendars"), "0", find_principal,
        xml(207, &multistatus(&format!(
            "<response><href>/dav/calendars/</href><propstat><prop><current-user-principal><href>{PRINCIPAL}</href></current-user-principal></prop><status>HTTP/1.1 200 OK</status></propstat></response>"
        ))));
    fixture(root, HttpMethod::Propfind, &format!("{HOST}{PRINCIPAL}"), "0", dav::BODY_PRINCIPAL,
        xml(207, &multistatus(&format!(
            "<response><href>{PRINCIPAL}</href><propstat><prop>\
             <C:calendar-home-set><href>{HOME}</href></C:calendar-home-set>\
             <C:calendar-user-address-set><href>mailto:picard@enterprise.test</href></C:calendar-user-address-set>\
             </prop><status>HTTP/1.1 200 OK</status></propstat></response>"
        ))));
    fixture(root, HttpMethod::Propfind, &format!("{HOST}{HOME}"), "1", dav::BODY_LIST_CALENDARS,
        xml(207, &multistatus(&format!(
            "<response><href>{HOME}</href><propstat><prop><resourcetype><collection/></resourcetype><displayname><![CDATA[Jean-Luc Picard]]></displayname></prop><status>HTTP/1.1 200 OK</status></propstat></response>\
             <response><href>{BRIDGE}</href><propstat><prop><resourcetype><collection/><C:calendar/></resourcetype>\
             <displayname><![CDATA[Bridge Duty]]></displayname>\
             <C:supported-calendar-component-set><C:comp name=\"VEVENT\"/></C:supported-calendar-component-set>\
             <C:calendar-timezone><![CDATA[BEGIN:VCALENDAR\r\nBEGIN:VTIMEZONE\r\nTZID:America/Los_Angeles\r\nEND:VTIMEZONE\r\nEND:VCALENDAR\r\n]]></C:calendar-timezone>\
             </prop><status>HTTP/1.1 200 OK</status></propstat></response>\
             <response><href>{HOME}Inbox/</href><propstat><prop><resourcetype><collection/><C:schedule-inbox/></resourcetype><displayname><![CDATA[Inbox]]></displayname></prop><status>HTTP/1.1 200 OK</status></propstat></response>\
             <response><href>{HOME}Outbox/</href><propstat><prop><resourcetype><collection/><C:schedule-outbox/></resourcetype><displayname><![CDATA[Outbox]]></displayname></prop><status>HTTP/1.1 200 OK</status></propstat></response>"
        ))));
}

async fn run(playback: &Path, store: &Path) -> FetchSummary {
    std::env::set_var(PLAYBACK_ENV, playback);
    let db = RawDb::open(&db_path_for(store)).await.expect("open store");
    let summary = caldav::fetch(caldav::FetchOptions {
        db: db.clone(),
        server_url: format!("{HOST}/"),
        calendars: Vec::new(),
        latchkey: LatchkeySettings::default(),
        progress: Default::default(),
        control: Default::default(),
    })
    .await;
    db.commit_all("test").await.expect("commit");
    db.close().await;
    std::env::remove_var(PLAYBACK_ENV);
    summary.expect("caldav fetch under playback")
}

async fn scalar(store: &Path, sql: &'static str) -> Option<String> {
    let db = RawDb::open(&db_path_for(store))
        .await
        .expect("reopen store");
    let v: Option<String> = sqlx::query_scalar(sql)
        .fetch_optional(db.pool())
        .await
        .expect("query")
        .flatten();
    db.close().await;
    v
}

/// Discovery finds the calendar the way Fastmail makes you find it, the
/// first sync stores every event, and the next applies a deletion and an
/// edit from the token it left.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn discovers_through_well_known_and_syncs_incrementally() {
    let d = tempfile::tempdir().expect("tempdir");
    let (one, two, store) = (
        d.path().join("one"),
        d.path().join("two"),
        d.path().join("store"),
    );
    std::fs::create_dir_all(&store).unwrap();
    let bridge = format!("{HOST}{BRIDGE}");
    for root in [&one, &two] {
        account_fixtures(root);
    }
    fixture(
        &one,
        HttpMethod::Report,
        &bridge,
        "1",
        &dav::body_sync_collection(""),
        xml(
            207,
            &multistatus(&format!(
                "{}{}<sync-token>data:,100</sync-token>",
                resource(&format!("{BRIDGE}staff.ics"), "\"s1\"", STAFF),
                resource(&format!("{BRIDGE}reception.ics"), "\"r1\"", RECEPTION),
            )),
        ),
    );
    let staff_v2 = STAFF.replace("Senior staff briefing", "Senior staff briefing (Deck 8)");
    fixture(&two, HttpMethod::Report, &bridge, "1", &dav::body_sync_collection("data:,100"),
        xml(207, &multistatus(&format!(
            "{}<response><href>{BRIDGE}reception.ics</href><status>HTTP/1.1 404 Not Found</status></response><sync-token>data:,101</sync-token>",
            resource(&format!("{BRIDGE}staff.ics"), "\"s2\"", &staff_v2),
        ))));

    // "Test connection" reaches the same account the way the download
    // does, and offers its one calendar — not the scheduling boxes.
    std::env::set_var(PLAYBACK_ENV, &one);
    let config: datalib_etl_calendar_config::CalendarConfig =
        serde_json::from_value(serde_json::json!({"caldav": {"server_url": format!("{HOST}/")}}))
            .unwrap();
    let report = datalib_etl_calendar::probe::probe(&config).await;
    std::env::remove_var(PLAYBACK_ENV);
    let report = report.expect("probe under playback");
    assert_eq!(
        report.account.address.as_deref(),
        Some("picard@enterprise.test")
    );
    let names: Vec<&str> = report.items.iter().map(|i| i.path.as_str()).collect();
    assert_eq!(names, vec!["Bridge Duty"]);

    let first = run(&one, &store).await;
    assert_eq!(
        (first.calendars, first.events_new, first.errors),
        (1, 2, 0),
        "{first:?}"
    );
    assert_eq!(
        scalar(&store, "SELECT display_name FROM calendars")
            .await
            .as_deref(),
        Some("Bridge Duty")
    );
    assert_eq!(
        scalar(&store, "SELECT time_zone FROM calendars")
            .await
            .as_deref(),
        Some("America/Los_Angeles")
    );
    assert_eq!(
        scalar(&store, "SELECT login FROM accounts")
            .await
            .as_deref(),
        Some("picard@enterprise.test")
    );
    assert_eq!(
        scalar(&store, "SELECT sync_token FROM calendars")
            .await
            .as_deref(),
        Some("data:,100")
    );
    assert_eq!(
        scalar(
            &store,
            "SELECT id FROM ics_objects WHERE uid = 'tng-reception@enterprise.test'"
        )
        .await
        .as_deref(),
        Some("2c1f4e0a-bridge#tng-reception@enterprise.test"),
    );

    let second = run(&two, &store).await;
    assert_eq!(
        (second.events_updated, second.events_deleted, second.errors),
        (1, 1, 0),
        "{second:?}"
    );
    assert_eq!(
        scalar(&store, "SELECT CAST(count(*) AS TEXT) FROM ics_objects")
            .await
            .as_deref(),
        Some("1")
    );
    assert!(scalar(
        &store,
        "SELECT json_extract(payload, '$.ics') FROM ics_objects"
    )
    .await
    .unwrap()
    .contains("(Deck 8)"));
    assert_eq!(
        scalar(&store, "SELECT sync_token FROM calendars")
            .await
            .as_deref(),
        Some("data:,101")
    );
}
