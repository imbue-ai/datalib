//! A TNG account's Todoist, as playback tapes. The envelopes and field
//! names follow Todoist's API v1 reference
//! (developer.todoist.com/api/v1, read 2026-10-26); none was checked
//! against a live account, and the values are made up.
//!
//! [`Edition::Before`] is the first sync: a full `/sync`, the
//! incremental one that follows it, and the tasks completed since the
//! account joined, read in three windows, the middle one over two pages.
//! [`Edition::After`] is a week later: one task completed, one deleted,
//! one added with a comment, a project renamed, and the new week of
//! completed tasks. Each tape also answers the token its own last answer
//! carried with nothing changed, which is what a run after it asks.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use datalib_etl_todoist::ingest::api;
use datalib_etl_todoist::ingest::windows;
use datalib_etl_web::http::{HttpRequest, HttpResponse};
use datalib_etl_web::synthesize::{json_response, write_fixture};
use datalib_time::IsoOffsetTimestamp;
use serde_json::{json, Value};

pub const PICARD: &str = "1701";
pub const LAFORGE: &str = "1702";

pub const INBOX: &str = "6InboxPicard";
pub const REFIT: &str = "6RefitProject";
pub const AWAY: &str = "6AwayMissions";
pub const ENGINEERING: &str = "6SecEngineering";
pub const BRIDGE: &str = "6SecBridge";

pub const COILS: &str = "6TaskCoils";
pub const CONDUIT: &str = "6TaskConduit";
pub const CHAIR: &str = "6TaskChair";
pub const TEA: &str = "6TaskTea";
pub const FARPOINT: &str = "6TaskFarpoint";
pub const BRIEFING: &str = "6TaskBriefing";

pub const PHYSICAL: &str = "6DonePhysical";
pub const REPORT: &str = "6DoneReport";
pub const BANDI: &str = "6DoneBandi";

pub const JOINED: &str = "2026-03-02T09:00:00.000000Z";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Edition {
    Before,
    After,
}

impl Edition {
    /// The instant the run is pinned to.
    pub fn now(self) -> IsoOffsetTimestamp {
        let s = match self {
            Edition::Before => "2026-10-01T00:00:00Z",
            Edition::After => "2026-10-08T00:00:00Z",
        };
        datalib_time::parse_with_assumed_utc(s).unwrap()
    }

    fn dir(self) -> &'static str {
        match self {
            Edition::Before => "before",
            Edition::After => "after",
        }
    }
}

pub fn put(root: &Path, req: HttpRequest, resp: HttpResponse) {
    write_fixture(root, &req, &resp).expect("write fixture");
}

pub fn status(code: u16, body: &str) -> HttpResponse {
    HttpResponse {
        status: code,
        headers: BTreeMap::new(),
        body: body.as_bytes().to_vec(),
        duration_ms: 0,
    }
}

pub fn user() -> Value {
    json!({
        "id": PICARD, "email": "picard@enterprise.test", "full_name": "Jean-Luc Picard",
        "joined_at": JOINED, "inbox_project_id": INBOX, "lang": "en", "is_premium": true,
        "tz_info": {"timezone": "Europe/Paris", "gmt_string": "+02:00", "hours": 2, "minutes": 0,
                    "is_dst": 1},
        "karma": 4200, "karma_trend": "up",
        "token": "0123456789abcdef0123456789abcdef01234567",
        "websocket_url": "wss://ws.todoist.com/ws?token=enterprise",
        "urgent_reminder_device": null,
        "avatar_big": "https://dcff1xvirvpfp.cloudfront.net/picard_big.jpg"
    })
}

fn user_later() -> Value {
    let mut u = user();
    u["karma"] = json!(4350);
    u
}

fn project(id: &str, name: &str, order: i64) -> Value {
    json!({
        "id": id, "name": name, "description": "", "color": "charcoal", "parent_id": null,
        "child_order": order, "is_collapsed": false, "shared": id == REFIT,
        "can_assign_tasks": id == REFIT, "is_deleted": false, "is_archived": false,
        "is_favorite": id == REFIT, "view_style": "list", "inbox_project": id == INBOX,
        "folder_id": null, "created_at": "2026-03-02T09:00:00Z",
        "updated_at": "2026-09-01T09:00:00Z"
    })
}

fn section(id: &str, name: &str, order: i64) -> Value {
    json!({
        "id": id, "name": name, "description": null, "project_id": REFIT,
        "section_order": order, "is_collapsed": false, "user_id": PICARD, "is_deleted": false,
        "is_archived": false, "archived_at": null, "added_at": "2026-03-03T09:00:00.000000Z",
        "updated_at": "2026-03-03T09:00:00.000000Z"
    })
}

#[allow(clippy::too_many_arguments)]
fn task(
    id: &str,
    project: &str,
    section: Option<&str>,
    parent: Option<&str>,
    content: &str,
    order: i64,
    labels: &[&str],
    added: &str,
) -> Value {
    json!({
        "id": id, "user_id": PICARD, "project_id": project, "content": content,
        "description": "", "priority": 1, "due": null, "deadline": null,
        "parent_id": parent, "child_order": order, "section_id": section, "day_order": -1,
        "is_collapsed": false, "labels": labels, "added_by_uid": PICARD,
        "assigned_by_uid": null, "responsible_uid": null, "checked": false,
        "is_deleted": false, "added_at": added, "updated_at": added, "completed_at": null,
        "duration": null
    })
}

fn coils() -> Value {
    let mut t = task(
        COILS,
        REFIT,
        Some(ENGINEERING),
        None,
        "Recalibrate the warp coils",
        1,
        // Todoist promises no order for a task's labels.
        &["warp", "engineering"],
        "2026-09-20T08:00:00.000000Z",
    );
    t["priority"] = json!(4);
    t["description"] = json!("Check the **dilithium** matrix first.");
    t["due"] = json!({"date": "2026-10-03", "timezone": null, "string": "Oct 3",
                      "lang": "en", "is_recurring": false});
    t["responsible_uid"] = json!(LAFORGE);
    t["assigned_by_uid"] = json!(PICARD);
    t
}

pub fn conduit() -> Value {
    task(
        CONDUIT,
        REFIT,
        Some(ENGINEERING),
        Some(COILS),
        "Replace the plasma conduit",
        1,
        &[],
        "2026-09-20T08:05:00.000000Z",
    )
}

fn chair() -> Value {
    task(
        CHAIR,
        REFIT,
        Some(BRIDGE),
        None,
        "Polish the captain's chair",
        1,
        &[],
        "2026-09-21T08:00:00.000000Z",
    )
}

fn tea() -> Value {
    let mut t = task(
        TEA,
        INBOX,
        None,
        None,
        "Tea, Earl Grey, hot",
        1,
        &[],
        "2026-09-22T08:00:00.000000Z",
    );
    t["description"] = json!("<script>x</script> & co");
    t
}

fn farpoint() -> Value {
    task(
        FARPOINT,
        AWAY,
        None,
        None,
        "Survey Farpoint station",
        1,
        &[],
        "2026-09-23T08:00:00.000000Z",
    )
}

fn note(id: &str, item: &str, by: &str, content: &str, at: &str) -> Value {
    json!({"id": id, "posted_uid": by, "item_id": item, "content": content,
           "file_attachment": null, "uids_to_notify": [], "is_deleted": false,
           "posted_at": at, "reactions": null})
}

fn collaborators() -> Value {
    json!([
        {"id": PICARD, "email": "picard@enterprise.test", "full_name": "Jean-Luc Picard",
         "timezone": "Europe/Paris", "image_id": null},
        {"id": LAFORGE, "email": "laforge@enterprise.test", "full_name": "Geordi La Forge",
         "timezone": "America/Chicago", "image_id": null}
    ])
}

/// A completed task, in the completed endpoint's shape.
fn done(id: &str, project: &str, content: &str, completed_at: &str) -> Value {
    json!({
        "id": id, "user_id": PICARD, "project_id": project, "section_id": null,
        "parent_id": null, "content": content, "description": "", "priority": 1,
        "labels": [], "checked": true, "is_deleted": false, "is_collapsed": false,
        "child_order": 1, "day_order": -1, "added_by_uid": PICARD, "assigned_by_uid": null,
        "responsible_uid": null, "completed_by_uid": PICARD, "completed_count": 1,
        "postponed_count": 0, "note_count": 0, "due": null, "deadline": null,
        "duration": null, "added_at": "2026-03-10T08:00:00Z", "completed_at": completed_at,
        "updated_at": completed_at
    })
}

fn empty_sync(token: &str, user: Value) -> Value {
    json!({
        "full_sync": false, "sync_token": token, "user": user, "projects": [],
        "sections": [], "items": [], "notes": [], "project_notes": [], "labels": [],
        "collaborators": [], "temp_id_mapping": {}
    })
}

/// The windows a run pinned at `edition`'s now asks for, newest first,
/// given what an earlier run covered.
pub fn windows_of(
    edition: Edition,
    held: &[datalib_etl_web::coverage::Span],
) -> Vec<(String, String)> {
    let now = windows::stamp(edition.now().to_unix_millis()).unwrap();
    let floor = windows::normalize(JOINED).unwrap();
    windows::to_read(&floor, &now, held)
        .into_iter()
        .map(|s| (s.lo, s.hi))
        .collect()
}

fn completed_page(t: &Path, window: &(String, String), cursor: Option<&str>, page: Value) {
    put(
        t,
        api::completed_request(&window.0, &window.1, cursor),
        json_response(&page),
    );
}

/// The whole tape for one edition, under `dir/<edition>`.
pub fn tape(dir: &Path, edition: Edition) -> PathBuf {
    let t = dir.join(edition.dir());
    match edition {
        Edition::Before => before(&t),
        Edition::After => after(&t),
    }
    t
}

fn before(t: &Path) {
    let full = json!({
        "full_sync": true,
        "full_sync_date_utc": "2026-09-30T23:58:00Z",
        "sync_token": "tok-1",
        "user": user(),
        "projects": [project(INBOX, "Inbox", 0), project(REFIT, "Enterprise refit", 1),
                     project(AWAY, "Away missions", 2)],
        "sections": [section(ENGINEERING, "Engineering", 1), section(BRIDGE, "Bridge", 2)],
        "items": [coils(), conduit(), chair(), tea(), farpoint()],
        "notes": [
            note("6NoteCoils1", COILS, LAFORGE, "The coils need a full day.",
                 "2026-09-20T09:00:00.000000Z"),
            note("6NoteCoils2", COILS, PICARD, "Make it so.", "2026-09-20T09:30:00.000000Z"),
        ],
        "project_notes": [
            {"id": "6PNoteRefit", "posted_uid": PICARD, "project_id": REFIT,
             "content": "The refit begins at Utopia Planitia.", "file_attachment": null,
             "uids_to_notify": [LAFORGE], "is_deleted": false,
             "posted_at": "2026-09-19T09:00:00.000000Z", "reactions": {"👍": [LAFORGE]}}
        ],
        "labels": [
            {"id": "2156154810", "name": "warp", "color": "red", "item_order": 0,
             "order_key": "a0", "is_deleted": false, "is_favorite": false},
            {"id": "2156154811", "name": "engineering", "color": "orange", "item_order": 1,
             "order_key": "a1", "is_deleted": false, "is_favorite": false}
        ],
        "collaborators": collaborators(),
        "temp_id_mapping": {}
    });
    put(t, api::sync_request("*"), json_response(&full));
    put(
        t,
        api::sync_request("tok-1"),
        json_response(&empty_sync("tok-2", user())),
    );
    // What a run after this one asks, with nothing changed upstream.
    put(
        t,
        api::sync_request("tok-2"),
        json_response(&empty_sync("tok-2", user())),
    );

    let w = windows_of(Edition::Before, &[]);
    assert_eq!(w.len(), 3, "the tape expects three windows: {w:?}");
    completed_page(
        t,
        &w[0],
        None,
        json!({"items": [done(PHYSICAL, INBOX, "Pass the Starfleet physical",
                              "2026-09-12T15:00:00.000000Z")],
               "next_cursor": null}),
    );
    completed_page(
        t,
        &w[1],
        None,
        json!({"items": [done(REPORT, AWAY, "Log the Farpoint report",
                              "2026-05-20T10:00:00.000000Z")],
               "next_cursor": "page-2.of-the-spring"}),
    );
    completed_page(
        t,
        &w[1],
        Some("page-2.of-the-spring"),
        json!({"items": [done(BANDI, AWAY, "Hail the Bandi", "2026-05-01T10:00:00.000000Z")],
               "next_cursor": null}),
    );
    completed_page(t, &w[2], None, json!({"items": [], "next_cursor": null}));
}

fn after(t: &Path) {
    let mut coils_done = coils();
    coils_done["checked"] = json!(true);
    coils_done["completed_at"] = json!("2026-10-05T10:00:00.000000Z");
    coils_done["updated_at"] = json!("2026-10-05T10:00:00.000000Z");
    let mut farpoint_gone = farpoint();
    farpoint_gone["is_deleted"] = json!(true);
    farpoint_gone["updated_at"] = json!("2026-10-06T10:00:00.000000Z");
    let briefing = task(
        BRIEFING,
        AWAY,
        None,
        None,
        "Brief the away team",
        2,
        &[],
        "2026-10-06T11:00:00.000000Z",
    );
    let mut away = project(AWAY, "Away missions, stardate 47457", 2);
    away["updated_at"] = json!("2026-10-06T12:00:00Z");
    let changes = json!({
        "full_sync": false,
        "sync_token": "tok-3",
        "user": user_later(),
        "projects": [away],
        "sections": [],
        "items": [coils_done, farpoint_gone, briefing],
        "notes": [note("6NoteBriefing", BRIEFING, LAFORGE, "Phasers on stun.",
                       "2026-10-06T11:30:00.000000Z")],
        "project_notes": [],
        "labels": [],
        "collaborators": [],
        "temp_id_mapping": {}
    });
    put(t, api::sync_request("tok-2"), json_response(&changes));
    put(
        t,
        api::sync_request("tok-3"),
        json_response(&empty_sync("tok-3", user_later())),
    );

    let held = [datalib_etl_web::coverage::Span::new(
        windows::normalize(JOINED).unwrap(),
        windows::stamp(Edition::Before.now().to_unix_millis()).unwrap(),
    )];
    let w = windows_of(Edition::After, &held);
    assert_eq!(w.len(), 1, "the tape expects one window: {w:?}");
    let mut coils_completed = done(
        COILS,
        REFIT,
        "Recalibrate the warp coils",
        "2026-10-05T10:00:00.000000Z",
    );
    coils_completed["section_id"] = json!(ENGINEERING);
    coils_completed["note_count"] = json!(2);
    completed_page(
        t,
        &w[0],
        None,
        json!({"items": [coils_completed], "next_cursor": null}),
    );
}

/// A tape where the credential is refused at once.
pub fn refused(dir: &Path) -> PathBuf {
    let t = dir.join("refused");
    put(
        &t,
        api::sync_request("*"),
        status(
            401,
            r#"{"error": "Invalid token", "error_code": 401, "error_tag": "AUTH_INVALID_TOKEN",
                "http_code": 401, "error_extra": {}}"#,
        ),
    );
    t
}
