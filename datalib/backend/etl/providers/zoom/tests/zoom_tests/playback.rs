//! The Zoom download end to end through HTTP playback of [`crate::tape`].

use std::path::Path;

use datalib_etl::control::DownloadControl;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_web::http::LatchkeySettings;
use datalib_etl_web::playback;
use datalib_etl_web::synthesize::json_response;
use datalib_etl_zoom::ingest::{self, api, db_path_for, FetchSummary, RawDb};
use datalib_etl_zoom_config::ZoomApi;
use serde_json::json;

use crate::tape::{self, Edition, AWAY_TEAM, BRIEFING, ENGINEERING, HOLODECK, USER_ID};

/// Every table the download writes, for a test to dump.
pub const TABLES: &[&str] = &[
    "users",
    "recordings",
    "reported_meetings",
    "participants",
    "summary_listings",
    "summaries",
    "recording_files",
    "coverage",
];

pub async fn run_with(
    store: &Path,
    tape: &Path,
    api: ZoomApi,
    cap: Option<u64>,
) -> anyhow::Result<FetchSummary> {
    let db = RawDb::open(&db_path_for(store)).await?;
    let r = playback::scope(
        tape,
        ingest::fetch(ingest::FetchOptions {
            db: db.clone(),
            latchkey: LatchkeySettings::default(),
            api,
            blob_size_limit_bytes: cap,
            today: tape::today(),
            progress: Default::default(),
            control: DownloadControl::default(),
            sealer: None,
        }),
    )
    .await;
    // Commit only what a run that returned Ok vouches for, as the
    // processor does.
    if r.is_ok() {
        db.commit_all("test").await?;
    }
    db.close().await;
    r
}

async fn run(store: &Path, tape: &Path) -> FetchSummary {
    run_with(store, tape, tape::api(), None)
        .await
        .expect("zoom fetch")
}

async fn scalar(store: &Path, sql: &'static str) -> i64 {
    let db = RawDb::open(&db_path_for(store)).await.unwrap();
    let n: i64 = sqlx::query_scalar(sql).fetch_one(db.pool()).await.unwrap();
    db.close().await;
    n
}

async fn text(store: &Path, sql: &'static str) -> Option<String> {
    let db = RawDb::open(&db_path_for(store)).await.unwrap();
    let v: Option<String> = sqlx::query_scalar(sql)
        .fetch_optional(db.pool())
        .await
        .unwrap()
        .flatten();
    db.close().await;
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_sync_copies_every_meeting_and_its_transcript() {
    let d = tempfile::tempdir().unwrap();
    let tape = tape::tape(d.path(), Edition::Before);
    let store = d.path().join("store");
    std::fs::create_dir_all(&store).unwrap();
    let s = run(&store, &tape).await;

    assert_eq!(s.recordings_listed, 2, "{s:?}");
    assert_eq!(s.meetings_reported, 2, "{s:?}");
    assert_eq!(s.summaries_listed, 1, "{s:?}");
    // The briefing's transcript, chat and timeline; not its video.
    assert_eq!((s.files, s.files_gone, s.files_skipped), (3, 1, 0), "{s:?}");
    assert_eq!(s.participants, 3, "{s:?}");
    assert_eq!(s.summaries, 1, "{s:?}");
    assert_eq!((s.failed, s.left), (0, 0), "{s:?}");

    let held = |key: &str| format!("{BRIEFING}#{key}");
    let db = RawDb::open(&db_path_for(&store)).await.unwrap();
    let edges: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT id, blake3 FROM recording_files ORDER BY id")
            .fetch_all(db.pool())
            .await
            .unwrap();
    let ids: Vec<&str> = edges.iter().map(|(id, _)| id.as_str()).collect();
    assert_eq!(
        ids,
        [
            format!("{ENGINEERING}#c-vtt"),
            held("a-chat"),
            held("a-vtt"),
            held("timeline@2026-10-05T09:01:00Z")
        ]
    );
    assert!(
        edges[0].1.is_none(),
        "Zoom had lost the engineering transcript"
    );
    let vtt = edges.iter().find(|(id, _)| *id == held("a-vtt")).unwrap();
    let bytes = db
        .cas()
        .get(vtt.1.as_deref().unwrap())
        .await
        .unwrap()
        .unwrap()
        .bytes;
    assert_eq!(String::from_utf8(bytes).unwrap(), tape::BRIEFING_VTT);
    let timeline = edges
        .iter()
        .find(|(id, _)| *id == held("timeline@2026-10-05T09:01:00Z"))
        .unwrap();
    assert!(timeline.1.is_some(), "the redirect was followed");

    let coverage: Vec<(String, String, String)> =
        sqlx::query_as("SELECT scope, lo, hi FROM coverage ORDER BY scope")
            .fetch_all(db.pool())
            .await
            .unwrap();
    assert_eq!(
        coverage,
        ["recordings", "reports", "summaries"].map(|l| (
            format!("{l}:{USER_ID}"),
            "2026-09-01".to_string(),
            "2026-10-20".to_string()
        ))
    );
    db.close().await;

    // Who attended the briefing, both pages; nobody for the holodeck,
    // and why.
    assert_eq!(
        scalar(
            &store,
            "SELECT json_array_length(payload, '$.participants') FROM participants \
             WHERE id = 'nCC1701+D/briefing=='"
        )
        .await,
        5
    );
    let why = text(
        &store,
        "SELECT json_extract(payload, '$.unavailable') FROM participants \
         WHERE id = '//Holodeck3Session=='",
    )
    .await
    .unwrap();
    assert!(why.contains("Zoom code 3001"), "{why}");
    let _ = HOLODECK;
}

/// Nothing changed upstream, so a second sync lists the refresh window
/// again and fetches nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_sync_of_an_unchanged_account_owes_nothing() {
    let d = tempfile::tempdir().unwrap();
    let tape = tape::tape(d.path(), Edition::Before);
    let store = d.path().join("store");
    std::fs::create_dir_all(&store).unwrap();
    run(&store, &tape).await;
    let s = run(&store, &tape).await;
    assert_eq!(
        (s.files, s.files_gone, s.participants, s.summaries, s.failed),
        (0, 0, 0, 0, 0),
        "{s:?}"
    );
    assert_eq!(s.windows, 6, "two windows of each listing: {s:?}");
}

/// The briefing's transcript and summary were edited and a meeting was
/// added: exactly those are fetched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_sync_fetches_what_moved_and_what_is_new() {
    let d = tempfile::tempdir().unwrap();
    let store = d.path().join("store");
    std::fs::create_dir_all(&store).unwrap();
    run(&store, &tape::tape(d.path(), Edition::Before)).await;
    let s = run(&store, &tape::tape(d.path(), Edition::After)).await;
    // The edited transcript and the debrief's.
    assert_eq!((s.files, s.files_gone), (2, 0), "{s:?}");
    assert_eq!(s.participants, 1, "the debrief's: {s:?}");
    assert_eq!(s.summaries, 1, "the edited one: {s:?}");

    let summary = text(
        &store,
        "SELECT json_extract(payload, '$.summary_content') FROM summaries",
    )
    .await
    .unwrap();
    assert!(summary.contains("two hours"), "{summary}");
    assert_eq!(
        scalar(
            &store,
            "SELECT COUNT(*) FROM recordings WHERE id = 'AwayTeam/Debrief=='"
        )
        .await,
        1
    );
    let _ = AWAY_TEAM;
}

/// The video is asked for, and is over the size limit: it is skipped
/// with a warning and stays owed, and the rest is fetched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_video_over_the_size_limit_is_skipped() {
    let d = tempfile::tempdir().unwrap();
    let tape = tape::tape(d.path(), Edition::Before);
    let store = d.path().join("store");
    std::fs::create_dir_all(&store).unwrap();
    let api = ZoomApi {
        media: true,
        ..tape::api()
    };
    let s = run_with(&store, &tape, api.clone(), Some(1_000_000))
        .await
        .unwrap();
    assert_eq!((s.files, s.files_skipped), (3, 1), "{s:?}");
    let s = run_with(&store, &tape, api.clone(), Some(1_000_000))
        .await
        .unwrap();
    assert_eq!(s.files_skipped, 1, "still owed, still skipped: {s:?}");

    // A raised limit fetches it.
    let s = run_with(&store, &tape, api, None).await.unwrap();
    assert_eq!((s.files, s.files_skipped), (1, 0), "{s:?}");
}

/// An account without a paid plan has no reports: the listing is a
/// problem, and the recordings, transcripts and summaries still come.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_report_zoom_will_not_give_costs_only_the_report() {
    let d = tempfile::tempdir().unwrap();
    let tape = tape::tape(d.path(), Edition::Before);
    let paid_only = tape::error(400, 200, "Only available for Paid account.");
    for w in [tape::W1, tape::W2] {
        let url = api::listing_url(api::Listing::Reports, USER_ID, w.0, w.1, None);
        tape::put(&tape, api::req_json(&url), paid_only.clone());
    }
    let store = d.path().join("store");
    std::fs::create_dir_all(&store).unwrap();
    let s = run(&store, &tape).await;
    assert_eq!(s.meetings_reported, 0, "{s:?}");
    assert_eq!(s.files, 3, "{s:?}");
    assert_eq!(s.summaries, 1, "{s:?}");
    assert_eq!(
        scalar(
            &store,
            "SELECT COUNT(*) FROM problems WHERE scope_key LIKE 'listing:reports%'"
        )
        .await,
        1
    );
}

/// No `api.user`, and a sign-in with no user of its own (a
/// Server-to-Server app): the account's only user is the one copied.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn with_no_user_named_an_account_s_only_user_is_copied() {
    let d = tempfile::tempdir().unwrap();
    let tape = tape::tape(d.path(), Edition::Before);
    tape::put(
        &tape,
        api::req_json(&api::user_url("me")),
        tape::error(400, 124, "Invalid access token."),
    );
    tape::put(
        &tape,
        api::req_json(&api::users_url()),
        json_response(&json!({"page_size": 2, "next_page_token": "", "users": [tape::user()]})),
    );
    let store = d.path().join("store");
    std::fs::create_dir_all(&store).unwrap();
    let api = ZoomApi {
        user: None,
        ..tape::api()
    };
    let s = run_with(&store, &tape, api, None).await.unwrap();
    assert_eq!(s.recordings_listed, 2, "{s:?}");

    // Two users and none named: the run says to name one.
    tape::put(
        &tape,
        api::req_json(&api::users_url()),
        json_response(&json!({"page_size": 2, "next_page_token": "x",
                              "users": [tape::user(), tape::user()]})),
    );
    let api = ZoomApi {
        user: None,
        ..tape::api()
    };
    let e = run_with(&store, &tape, api, None).await.unwrap_err();
    assert!(format!("{e:#}").contains("set `api.user`"), "{e:#}");
}
