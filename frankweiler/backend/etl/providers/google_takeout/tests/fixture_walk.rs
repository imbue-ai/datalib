//! End-to-end fixture walk: point the extractor at the checked-in
//! TNG-themed Takeout tree and assert each feed lands the rows
//! `docs/dev/google_takeout_ingestion.md` promises.

use std::path::PathBuf;

use frankweiler_etl_google_takeout::download::{self, FetchOptions, RawDb, SyncFlags};

fn fixture_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/Takeout")
}

async fn run_all() -> (tempfile::TempDir, download::FetchSummary, PathBuf) {
    let work = tempfile::tempdir().unwrap();
    let db_path = work.path().join("gt.doltlite_db");
    let db = RawDb::open(&db_path).await.unwrap();
    let pool = db.pool().clone();
    let summary = download::fetch(FetchOptions {
        db_path: db_path.clone(),
        db: Some(db),
        input_path: fixture_root(),
        sync: SyncFlags::all(),
        ..Default::default()
    })
    .await
    .unwrap();
    pool.close().await;
    (work, summary, db_path)
}

#[tokio::test(flavor = "multi_thread")]
async fn maps_reviews_lands_two_rows() {
    let (_work, summary, db_path) = run_all().await;
    assert_eq!(summary.maps_reviews, 2);
    let db = RawDb::open(&db_path).await.unwrap();
    let rows = db.load_payloads("maps_reviews").await.unwrap();
    assert_eq!(rows.len(), 2);
    let names: Vec<String> = rows
        .iter()
        .filter_map(|v| {
            v.get("properties")
                .and_then(|p| p.get("location"))
                .and_then(|l| l.get("name"))
                .and_then(|n| n.as_str())
                .map(str::to_string)
        })
        .collect();
    assert!(names.iter().any(|n| n == "Ten Forward"));
    assert!(names.iter().any(|n| n == "Resort Lounge"));
}

#[tokio::test(flavor = "multi_thread")]
async fn maps_saved_places_handles_ftid_and_cid() {
    let (_work, summary, _db_path) = run_all().await;
    assert_eq!(summary.maps_saved_places, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn maps_photo_lands_row_and_blob() {
    let (_work, summary, db_path) = run_all().await;
    assert_eq!(summary.maps_photos, 1);
    let db = RawDb::open(&db_path).await.unwrap();
    let rows = db.load_payloads("maps_photos").await.unwrap();
    assert_eq!(rows.len(), 1);
    // blake3 column populated from JPEG bytes.
    let blake3: Option<String> = sqlx::query_scalar("SELECT blake3 FROM maps_photos WHERE id = ?")
        .bind("2026-06-04-tenfwd")
        .fetch_one(db.pool())
        .await
        .unwrap();
    let blake3 = blake3.expect("blake3 populated");
    assert_eq!(blake3.len(), 64);
    // CAS has the bytes.
    let exists: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM cas_objects WHERE blake3 = ?)")
            .bind(&blake3)
            .fetch_one(db.cas().pool())
            .await
            .unwrap();
    assert!(exists, "photo bytes in CAS");
}

#[tokio::test(flavor = "multi_thread")]
async fn youtube_subscriptions_handles_quoted_titles() {
    let (_work, summary, db_path) = run_all().await;
    assert_eq!(summary.youtube_subscriptions, 3);
    let db = RawDb::open(&db_path).await.unwrap();
    let title: String = sqlx::query_scalar(
        "SELECT channel_title FROM youtube_subscriptions WHERE id = 'UCriker002'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(title, "Riker, William T.");
}

#[tokio::test(flavor = "multi_thread")]
async fn youtube_watch_history_parses_cells_and_timestamps() {
    let (_work, summary, db_path) = run_all().await;
    assert_eq!(summary.youtube_watch_history, 2);
    let db = RawDb::open(&db_path).await.unwrap();
    // video_id promoted column populated for each row.
    let count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM youtube_watch_history WHERE video_id IS NOT NULL")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(count, 2);
    let when: Option<String> = sqlx::query_scalar(
        "SELECT when_ts FROM youtube_watch_history WHERE video_id = 'trekS01E01'",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    // Parsed PDT-relative; the rfc3339 wallclock is the local 11:48
    // turned into a -07:00 offset.
    assert!(when.unwrap().starts_with("2026-06-04T11:48:37"));
}

#[tokio::test(flavor = "multi_thread")]
async fn google_chat_lands_groups_users_messages_and_attachments() {
    let (_work, summary, db_path) = run_all().await;
    assert_eq!(summary.chat_groups, 1);
    assert_eq!(summary.chat_users, 1);
    assert_eq!(summary.chat_messages, 2);
    assert_eq!(summary.chat_attachments, 1);
    let db = RawDb::open(&db_path).await.unwrap();
    // The DM group key is the takeout directory name verbatim.
    let group_ids: Vec<String> = sqlx::query_scalar("SELECT id FROM chat_groups ORDER BY id")
        .fetch_all(db.pool())
        .await
        .unwrap();
    assert_eq!(group_ids, vec!["DM TNG-BRIDGE"]);
    // Attachment edge row exists with the CAS blake3 set.
    let blake3: Option<String> = sqlx::query_scalar("SELECT blake3 FROM chat_attachments LIMIT 1")
        .fetch_one(db.pool())
        .await
        .unwrap();
    let blake3 = blake3.expect("blake3 set");
    let bytes: Vec<u8> = sqlx::query_scalar("SELECT bytes FROM cas_objects WHERE blake3 = ?")
        .bind(&blake3)
        .fetch_one(db.cas().pool())
        .await
        .unwrap();
    let s = String::from_utf8(bytes).unwrap();
    assert!(s.contains("Course 314"));
}

#[tokio::test(flavor = "multi_thread")]
async fn gemini_apps_lands_two_cells_and_one_attachment() {
    let (_work, summary, db_path) = run_all().await;
    assert_eq!(summary.gemini_activity, 2);
    assert_eq!(summary.gemini_attachments, 1);
    let db = RawDb::open(&db_path).await.unwrap();
    let when_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM gemini_activity WHERE when_ts IS NOT NULL")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(when_count, 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn second_run_skips_via_file_checkpoint() {
    let (work, summary1, db_path) = run_all().await;
    assert!(summary1.maps_reviews > 0);
    assert!(summary1.youtube_watch_history > 0);

    // Same fixture root, freshly opened db pool — cursor rows from
    // the first run mean every file's fingerprint matches and the
    // walkers short-circuit.
    let db = RawDb::open(&db_path).await.unwrap();
    let pool = db.pool().clone();
    let summary2 = download::fetch(FetchOptions {
        db_path: db_path.clone(),
        db: Some(db),
        input_path: fixture_root(),
        sync: SyncFlags::all(),
        ..Default::default()
    })
    .await
    .unwrap();
    pool.close().await;
    let _ = work; // keep temp dir alive

    assert_eq!(summary2.maps_reviews, 0);
    assert_eq!(summary2.maps_saved_places, 0);
    assert_eq!(summary2.youtube_watch_history, 0);
    assert_eq!(summary2.youtube_subscriptions, 0);
    assert_eq!(summary2.chat_messages, 0);
    assert_eq!(summary2.gemini_activity, 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn sync_flags_default_disables_everything() {
    let work = tempfile::tempdir().unwrap();
    let db_path = work.path().join("gt.doltlite_db");
    let db = RawDb::open(&db_path).await.unwrap();
    let pool = db.pool().clone();
    // Default SyncFlags has every feed off.
    let summary = download::fetch(FetchOptions {
        db_path: db_path.clone(),
        db: Some(db),
        input_path: fixture_root(),
        sync: SyncFlags::default(),
        ..Default::default()
    })
    .await
    .unwrap();
    pool.close().await;
    assert_eq!(summary.maps_reviews, 0);
    assert_eq!(summary.youtube_subscriptions, 0);
    assert_eq!(summary.chat_messages, 0);
    assert_eq!(summary.gemini_activity, 0);
}
