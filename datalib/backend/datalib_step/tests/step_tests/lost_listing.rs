//! A listing that got no answer ends the ingest `incomplete`, however the
//! provider took it. Google Calendar recorded the failure, went on to the
//! next calendar and ended `succeeded`, so a source whose one calendar was
//! lost read as a source with nothing in it.
use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use datalib_etl_calendar::ingest::google::{calendar_list_url, events_url, HTTP_SERVICE};
use datalib_etl_web::http::{HttpRequest, HttpResponse};
use datalib_etl_web::synthesize::{json_response, write_fixture};
use serde_json::{json, Value};

use crate::interrupt::doltlite;

const STEP: &str = "ship-calendar/ingest";
const BRIDGE: &str = "bridge@group.calendar.google.com";
const AWAY: &str = "away@group.calendar.google.com";

fn fixture(playback: &Path, url: &str, resp: HttpResponse) {
    let req = HttpRequest::get(HTTP_SERVICE, url).header("Accept", "application/json");
    write_fixture(playback, &req, &resp).unwrap();
}

fn account(playback: &Path) {
    let calendars = [(BRIDGE, "Bridge duty"), (AWAY, "Away team")]
        .map(|(id, summary)| json!({"id": id, "summary": summary}));
    fixture(
        playback,
        &calendar_list_url(None),
        json_response(&json!({"kind": "calendar#calendarList", "items": calendars})),
    );
}

fn events(playback: &Path, calendar: &str, id: &str) {
    fixture(
        playback,
        &events_url(calendar, None, None),
        json_response(&json!({
            "kind": "calendar#events",
            "nextSyncToken": "t1",
            "items": [{"id": id, "status": "confirmed", "summary": "Staff meeting"}],
        })),
    );
}

fn ingest(root: &Path, playback: &Path) -> Output {
    let bin = std::env::var_os("DATALIB_STEP_BIN").expect("DATALIB_STEP_BIN");
    fs::create_dir_all(root).unwrap();
    Command::new(bin)
        .env("DATALIB_DAG_PARAMS", json!({"google": {}}).to_string())
        .env("DATALIB_DAG_STEP", STEP)
        .args(["run", "ingest", "calendar"])
        .env("DATALIB_DAG_DATA_ROOT", root)
        .env("DATALIB_DAG_NOW", "2369-03-01T00:00:00+00:00")
        .env("DATALIB_HTTP_PLAYBACK", playback)
        .output()
        .expect("run datalib-step")
}

fn outcome(out: &Output) -> Value {
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["event"] == "outcome")
        .expect("an outcome line")
}

#[test]
fn a_listing_that_got_no_answer_ends_the_ingest_incomplete_with_the_rest_committed() {
    let d = tempfile::tempdir().unwrap();
    let (root, playback) = (d.path().join("root"), d.path().join("playback"));
    account(&playback);
    // The tape has no answer for the bridge calendar's events.
    events(&playback, AWAY, "away1");

    let out = ingest(&root, &playback);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !out.status.success(),
        "half the calendars is not a success:\n{stderr}"
    );
    let said = outcome(&out);
    assert_eq!(said["failure"], "incomplete", "{said}\n{stderr}");
    assert_eq!(said["outputs"].as_array().map(Vec::len), Some(1), "{said}");
    assert!(
        stderr.contains("calendar Bridge duty"),
        "the step names what it did not get:\n{stderr}"
    );

    let db = root.join(STEP).join("entities.doltlite_db");
    assert_eq!(
        doltlite(&db, "SELECT calendar_id FROM dolt_at_google_events('HEAD')"),
        [AWAY],
        "what answered is committed"
    );

    // The next run gets its answer, and is done.
    events(&playback, BRIDGE, "bridge1");
    fixture(
        &playback,
        &events_url(AWAY, Some("t1"), None),
        json_response(&json!({"kind": "calendar#events", "nextSyncToken": "t2", "items": []})),
    );
    let out = ingest(&root, &playback);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}
