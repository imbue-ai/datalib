//! A TNG Zoom account, as playback fixtures. The envelopes and field
//! names follow Zoom's published API reference (INGEST.md cites the
//! pages); no live account has answered any of it, and the values are
//! made up.
//!
//! Captain Picard's meetings, listed from 2026-09-01 to "today",
//! 2026-10-20:
//!
//! - the senior staff briefing: recorded, with a video, a transcript, a
//!   chat file and a timeline (whose link redirects); its participants
//!   span two pages, one of them rejoining; the AI Companion summarized
//!   it;
//! - an engineering status meeting: recorded, but Zoom has lost its
//!   transcript by the time it is downloaded;
//! - a holodeck session: not recorded, so only the report lists it, and
//!   Zoom will not say who came.
//!
//! The recordings listing of the newest window spans two pages.
//! [`Edition::After`] edits the briefing's transcript and its summary,
//! and adds an away-team debrief.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use chrono::NaiveDate;
use datalib_etl_web::http::{HttpRequest, HttpResponse};
use datalib_etl_web::synthesize::{json_response, write_fixture};
use datalib_etl_zoom::ingest::api::{self, Listing};
use datalib_etl_zoom_config::ZoomApi;
use serde_json::{json, Value};

pub const USER: &str = "picard@enterprise.example";
pub const USER_ID: &str = "z8PicardUid0001";
pub const BRIEFING: &str = "nCC1701+D/briefing==";
pub const ENGINEERING: &str = "Eng1neering+Status==";
pub const HOLODECK: &str = "//Holodeck3Session==";
pub const AWAY_TEAM: &str = "AwayTeam/Debrief==";

/// The newest window, the oldest, and the one a later run's refresh
/// reaches back to.
pub const W1: (&str, &str) = ("2026-09-23", "2026-10-20");
pub const W2: (&str, &str) = ("2026-09-01", "2026-09-23");
pub const W3: (&str, &str) = ("2026-09-20", "2026-09-23");

pub fn today() -> NaiveDate {
    NaiveDate::from_ymd_opt(2026, 10, 20).unwrap()
}

pub fn api() -> ZoomApi {
    ZoomApi {
        user: Some(USER.to_string()),
        since: Some("2026-09-01".to_string()),
        ..Default::default()
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Edition {
    Before,
    After,
}

pub const BRIEFING_VTT: &str = "WEBVTT\n\n\
1\n00:00:01.000 --> 00:00:04.500\nJean-Luc Picard: Number One, report.\n\n\
2\n00:00:05.000 --> 00:00:09.000\nWilliam Riker: All decks report ready, Captain.\n\n\
3\n00:00:09.500 --> 00:00:12.000\nWilliam Riker: Engineering asks for an hour on the warp core.\n\n\
4\n00:00:12.500 --> 00:00:16.000\nData: The anomaly is 0.4 light years off the port bow.\n\n\
5\n00:00:17.000 --> 00:00:19.000\nMake it so.\n";

pub const BRIEFING_VTT_EDITED: &str = "WEBVTT\n\n\
1\n00:00:01.000 --> 00:00:04.500\nJean-Luc Picard: Number One, report.\n\n\
2\n00:00:05.000 --> 00:00:09.000\nWilliam Riker: All decks report ready, Captain.\n\n\
3\n00:00:09.500 --> 00:00:12.000\nWilliam Riker: Engineering asks for an hour on the warp core.\n\n\
4\n00:00:12.500 --> 00:00:16.000\nData: The anomaly is 0.4 light years off the port bow.\n\n\
5\n00:00:17.000 --> 00:00:19.000\nJean-Luc Picard: Make it so. Engage.\n";

pub const AWAY_TEAM_VTT: &str = "WEBVTT\n\n\
1\n00:00:00.500 --> 00:00:03.000\nWorf: The away team is aboard.\n\n\
2\n00:00:03.500 --> 00:00:06.000\nBeverly Crusher: No injuries to report.\n";

const CHAT_TXT: &str =
    "09:02:11 From Geordi La Forge to Everyone:\n\tWarp core diagnostics attached.\n";
const TIMELINE_JSON: &str =
    r#"{"timeline":[{"ts":"00:00:01.000","users":[{"username":"Jean-Luc Picard"}]}]}"#;

pub fn put(root: &Path, req: HttpRequest, resp: HttpResponse) {
    write_fixture(root, &req, &resp).expect("write fixture");
}

pub fn bytes(status: u16, body: &[u8]) -> HttpResponse {
    HttpResponse {
        status,
        headers: BTreeMap::new(),
        body: body.to_vec(),
        duration_ms: 0,
    }
}

pub fn error(status: u16, code: i64, message: &str) -> HttpResponse {
    let mut r = json_response(&json!({"code": code, "message": message}));
    r.status = status;
    r
}

fn json_at(root: &Path, url: &str, body: Value) {
    put(root, api::req_json(url), json_response(&body));
}

pub fn user() -> Value {
    json!({"id": USER_ID, "first_name": "Jean-Luc", "last_name": "Picard",
           "display_name": "Jean-Luc Picard", "email": USER, "type": 2,
           "timezone": "UTC", "status": "active", "account_id": "acctNCC1701D",
           "last_login_time": "2026-10-19T08:00:00Z", "last_client_version": "6.2.5.1234(mac)"})
}

fn file(
    meeting: &str,
    id: Option<&str>,
    file_type: &str,
    ext: &str,
    size: Option<usize>,
    rtype: &str,
) -> Value {
    let key = id.map_or_else(|| file_type.to_ascii_lowercase(), str::to_string);
    let mut f = json!({
        "meeting_id": meeting,
        "recording_start": "2026-10-05T09:01:00Z",
        "recording_end": "2026-10-05T09:45:00Z",
        "file_type": file_type,
        "file_extension": ext,
        "download_url": format!("https://zoom.us/rec/download/{key}"),
    });
    if let Some(id) = id {
        f["id"] = json!(id);
        f["status"] = json!("completed");
        f["recording_type"] = json!(rtype);
        f["play_url"] = json!(format!("https://zoom.us/rec/play/{id}"));
    }
    if let Some(size) = size {
        f["file_size"] = json!(size);
    }
    f
}

fn briefing_vtt(edition: Edition) -> &'static str {
    match edition {
        Edition::Before => BRIEFING_VTT,
        Edition::After => BRIEFING_VTT_EDITED,
    }
}

pub fn briefing(edition: Edition) -> Value {
    json!({
        "uuid": BRIEFING, "id": 85017010, "account_id": "acctNCC1701D", "host_id": USER_ID,
        "topic": "Senior staff briefing", "type": "2", "start_time": "2026-10-05T09:00:00Z",
        "duration": 45, "recording_count": 4,
        "share_url": "https://zoom.us/rec/share/senior-staff-briefing",
        "recording_files": [
            file(BRIEFING, Some("a-mp4"), "MP4", "MP4", Some(734_003_200), "shared_screen_with_speaker_view"),
            file(BRIEFING, Some("a-vtt"), "TRANSCRIPT", "VTT", Some(briefing_vtt(edition).len()), "audio_transcript"),
            file(BRIEFING, Some("a-chat"), "CHAT", "TXT", Some(CHAT_TXT.len()), "chat_file"),
            file(BRIEFING, None, "TIMELINE", "JSON", None, "timeline"),
        ]
    })
}

pub fn engineering() -> Value {
    json!({
        "uuid": ENGINEERING, "id": 85017011, "account_id": "acctNCC1701D", "host_id": USER_ID,
        "topic": "Engineering status", "type": "8", "start_time": "2026-09-30T14:00:00Z",
        "duration": 20, "recording_count": 1,
        "recording_files": [
            file(ENGINEERING, Some("c-vtt"), "TRANSCRIPT", "VTT", Some(512), "audio_transcript"),
        ]
    })
}

pub fn away_team() -> Value {
    json!({
        "uuid": AWAY_TEAM, "id": 85017012, "account_id": "acctNCC1701D", "host_id": USER_ID,
        "topic": "Away team debrief", "type": "1", "start_time": "2026-10-18T16:00:00Z",
        "duration": 10, "recording_count": 1,
        "recording_files": [
            file(AWAY_TEAM, Some("d-vtt"), "TRANSCRIPT", "VTT", Some(AWAY_TEAM_VTT.len()), "audio_transcript"),
        ]
    })
}

fn reported(uuid: &str, topic: &str, start: &str, minutes: i64, people: i64) -> Value {
    json!({"uuid": uuid, "id": 85017000, "topic": topic, "type": 2, "start_time": start,
           "end_time": start, "duration": minutes, "participants_count": people,
           "user_name": "Jean-Luc Picard", "user_email": USER, "has_recording": true,
           "has_chat": false, "has_screen_share": false, "source": "Zoom"})
}

fn summary_listing(modified: &str) -> Value {
    json!({"meeting_uuid": BRIEFING, "meeting_id": 85017010,
           "meeting_topic": "Senior staff briefing", "meeting_host_id": USER_ID,
           "meeting_host_email": USER, "meeting_start_time": "2026-10-05T09:00:00Z",
           "meeting_end_time": "2026-10-05T09:45:00Z",
           "summary_start_time": "2026-10-05T09:00:00Z", "summary_end_time": "2026-10-05T09:45:00Z",
           "summary_created_time": "2026-10-05T09:50:00Z",
           "summary_last_modified_time": modified})
}

fn summary(edition: Edition) -> Value {
    let (modified, step) = match edition {
        Edition::Before => (
            "2026-10-05T09:50:00Z",
            "- **La Forge**\n  - Take the warp core offline for an hour",
        ),
        Edition::After => (
            "2026-10-19T10:00:00Z",
            "- **La Forge**\n  - Take the warp core offline for two hours",
        ),
    };
    let mut s = summary_listing(modified);
    s["summary_title"] = json!("Meeting summary for Senior staff briefing");
    s["summary_content"] = json!(format!(
        "## Key takeaways\n- An anomaly lies off the port bow.\n\n## Action items\n{step}"
    ));
    s["summary_doc_url"] = json!("https://docs.zoom.us/doc/briefing-summary");
    s
}

fn participant(name: &str, email: &str, seconds: i64) -> Value {
    json!({"id": "", "name": name, "user_id": format!("{}", name.len()), "user_email": email,
           "join_time": "2026-10-05T09:00:00Z", "leave_time": "2026-10-05T09:45:00Z",
           "duration": seconds, "failover": false, "status": "in_meeting", "internal_user": true})
}

fn page(key: &str, items: Vec<Value>, next: Option<&str>) -> Value {
    json!({"page_size": 300, "next_page_token": next.unwrap_or(""), key: items})
}

fn listing(root: &Path, listing: Listing, w: (&str, &str), token: Option<&str>, body: Value) {
    json_at(
        root,
        &api::listing_url(listing, USER_ID, w.0, w.1, token),
        body,
    );
}

/// Writes the tape for `edition` under `root` and returns it.
pub fn tape(root: &Path, edition: Edition) -> PathBuf {
    let root = root.join(match edition {
        Edition::Before => "before",
        Edition::After => "after",
    });
    let after = edition == Edition::After;
    json_at(&root, &api::user_url(USER), user());

    let mut newest = vec![briefing(edition)];
    if after {
        newest.push(away_team());
    }
    listing(
        &root,
        Listing::Recordings,
        W1,
        None,
        page("meetings", newest, Some("rec-p2")),
    );
    listing(
        &root,
        Listing::Recordings,
        W1,
        Some("rec-p2"),
        page("meetings", vec![engineering()], None),
    );
    listing(
        &root,
        Listing::Recordings,
        W2,
        None,
        page("meetings", vec![], None),
    );
    listing(
        &root,
        Listing::Recordings,
        W3,
        None,
        page("meetings", vec![], None),
    );

    let mut reports = vec![reported(
        BRIEFING,
        "Senior staff briefing",
        "2026-10-05T09:00:00Z",
        45,
        5,
    )];
    if after {
        reports.push(reported(
            AWAY_TEAM,
            "Away team debrief",
            "2026-10-18T16:00:00Z",
            10,
            3,
        ));
    }
    listing(
        &root,
        Listing::Reports,
        W1,
        None,
        page("meetings", reports, None),
    );
    let holodeck = reported(
        HOLODECK,
        "Holodeck 3: Dixon Hill",
        "2026-09-10T20:00:00Z",
        90,
        2,
    );
    listing(
        &root,
        Listing::Reports,
        W2,
        None,
        page("meetings", vec![holodeck], None),
    );
    listing(
        &root,
        Listing::Reports,
        W3,
        None,
        page("meetings", vec![], None),
    );

    let modified = if after {
        "2026-10-19T10:00:00Z"
    } else {
        "2026-10-05T09:50:00Z"
    };
    listing(
        &root,
        Listing::Summaries,
        W1,
        None,
        page("summaries", vec![summary_listing(modified)], None),
    );
    listing(
        &root,
        Listing::Summaries,
        W2,
        None,
        page("summaries", vec![], None),
    );
    listing(
        &root,
        Listing::Summaries,
        W3,
        None,
        page("summaries", vec![], None),
    );

    // The files, behind each meeting's download token.
    let with_token = |meeting: Value, token: &str| {
        let mut m = meeting;
        m["download_access_token"] = json!(token);
        m
    };
    json_at(
        &root,
        &api::meeting_recordings_url(BRIEFING),
        with_token(briefing(edition), "tok-briefing"),
    );
    json_at(
        &root,
        &api::meeting_recordings_url(ENGINEERING),
        with_token(engineering(), "tok-engineering"),
    );
    json_at(
        &root,
        &api::meeting_recordings_url(AWAY_TEAM),
        with_token(away_team(), "tok-away"),
    );
    let dl = |key: &str, token: &str| {
        api::req_download(&format!("https://zoom.us/rec/download/{key}"), token)
    };
    put(
        &root,
        dl("a-vtt", "tok-briefing"),
        bytes(200, briefing_vtt(edition).as_bytes()),
    );
    put(
        &root,
        dl("a-chat", "tok-briefing"),
        bytes(200, CHAT_TXT.as_bytes()),
    );
    let signed = "https://ssrweb.zoom.us/cmr/replay/timeline?signature=made-up";
    let mut redirect = bytes(302, b"");
    redirect.headers.insert("location".into(), signed.into());
    put(&root, dl("timeline", "tok-briefing"), redirect);
    put(
        &root,
        api::req_redirected(signed),
        bytes(200, TIMELINE_JSON.as_bytes()),
    );
    put(
        &root,
        dl("a-mp4", "tok-briefing"),
        bytes(200, b"not really a video"),
    );
    put(
        &root,
        dl("c-vtt", "tok-engineering"),
        error(404, 3322, "This meeting transcript does not exist."),
    );
    put(
        &root,
        dl("d-vtt", "tok-away"),
        bytes(200, AWAY_TEAM_VTT.as_bytes()),
    );

    let people = |token: Option<&str>| api::participants_url(BRIEFING, token);
    json_at(
        &root,
        &people(None),
        page(
            "participants",
            vec![
                participant("Jean-Luc Picard", USER, 1500),
                participant("William Riker", "riker@enterprise.example", 2700),
                participant("Data", "data@enterprise.example", 2700),
            ],
            Some("pp2"),
        ),
    );
    json_at(
        &root,
        &people(Some("pp2")),
        page(
            "participants",
            vec![
                participant("Jean-Luc Picard", USER, 1199),
                participant("Geordi La Forge", "laforge@enterprise.example", 2400),
            ],
            None,
        ),
    );
    json_at(
        &root,
        &api::participants_url(ENGINEERING, None),
        page(
            "participants",
            vec![
                participant("Jean-Luc Picard", USER, 1200),
                participant("Geordi La Forge", "laforge@enterprise.example", 1200),
            ],
            None,
        ),
    );
    json_at(
        &root,
        &api::participants_url(AWAY_TEAM, None),
        page(
            "participants",
            vec![
                participant("Jean-Luc Picard", USER, 600),
                participant("Worf", "worf@enterprise.example", 600),
                participant("Beverly Crusher", "crusher@enterprise.example", 600),
            ],
            None,
        ),
    );
    put(
        &root,
        api::req_json(&api::participants_url(HOLODECK, None)),
        error(404, 3001, "Meeting does not exist: //Holodeck3Session==."),
    );

    json_at(&root, &api::summary_url(BRIEFING), summary(edition));
    root
}
