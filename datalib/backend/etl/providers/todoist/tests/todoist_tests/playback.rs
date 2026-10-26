//! The Todoist download driven end to end through HTTP playback, on the
//! TNG tapes in [`crate::tape`].

use std::path::Path;

use datalib_etl::doltlite_raw::commit_run;
use datalib_etl_todoist::ingest::{self, db_path_for, FetchSummary, RawDb};
use datalib_etl_todoist_config::TodoistSync;
use datalib_etl_web::playback;
use serde_json::Value;

use crate::tape::{self, Edition};

/// Every table a run's result is compared on.
pub const TABLES: &[&str] = &[
    "user",
    "projects",
    "sections",
    "items",
    "notes",
    "project_notes",
    "labels",
    "collaborators",
    "completed_items",
    "sync_state",
    "coverage",
];

pub fn options(db: RawDb, edition: Edition) -> ingest::FetchOptions {
    ingest::FetchOptions {
        now: Some(edition.now()),
        ..ingest::FetchOptions::new(db)
    }
}

async fn run(store: &Path, tape: &Path, edition: Edition) -> anyhow::Result<FetchSummary> {
    run_with(store, tape, edition, TodoistSync::default()).await
}

async fn run_with(
    store: &Path,
    tape: &Path,
    edition: Edition,
    sync: TodoistSync,
) -> anyhow::Result<FetchSummary> {
    let db = RawDb::open(&db_path_for(store)).await.unwrap();
    let r = playback::scope(
        tape,
        ingest::fetch(ingest::FetchOptions {
            sync,
            ..options(db.clone(), edition)
        }),
    )
    .await;
    commit_run(db.pool(), "test").await.unwrap();
    db.close().await;
    r
}

async fn payload(store: &Path, table: &str, id: &str) -> Option<Value> {
    let db = RawDb::open(&db_path_for(store)).await.unwrap();
    // Audited: `table` is a name this test wrote.
    let sql = format!("SELECT json(payload) FROM \"{table}\" WHERE id = ?");
    let p: Option<Option<String>> = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
        .bind(id)
        .fetch_optional(db.pool())
        .await
        .unwrap();
    db.close().await;
    p.flatten().map(|p| serde_json::from_str(&p).unwrap())
}

async fn scalar(store: &Path, sql: &'static str) -> String {
    let db = RawDb::open(&db_path_for(store)).await.unwrap();
    let v: String = sqlx::query_scalar(sql).fetch_one(db.pool()).await.unwrap();
    db.close().await;
    v
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_sync_mirrors_the_account_and_every_completed_task() {
    let d = tempfile::tempdir().unwrap();
    let t = tape::tape(d.path(), Edition::Before);
    let store = d.path().join("store");
    let s = run(&store, &t, Edition::Before).await.unwrap();
    assert_eq!(
        s.syncs, 2,
        "a full sync is followed by an incremental one: {s:?}"
    );
    assert!(s.full_sync);
    assert_eq!(s.windows, 3, "{s:?}");
    assert_eq!(s.completed_written, 3, "{s:?}");
    assert_eq!(
        s.requests, 6,
        "two syncs and four pages of completed tasks: {s:?}"
    );

    let counts = scalar(
        &store,
        "SELECT (SELECT COUNT(*) FROM projects) || ',' || (SELECT COUNT(*) FROM sections) \
         || ',' || (SELECT COUNT(*) FROM items) || ',' || (SELECT COUNT(*) FROM notes) \
         || ',' || (SELECT COUNT(*) FROM project_notes) || ',' || (SELECT COUNT(*) FROM labels) \
         || ',' || (SELECT COUNT(*) FROM collaborators) || ',' || (SELECT COUNT(*) FROM user) \
         || ',' || (SELECT COUNT(*) FROM completed_items)",
    )
    .await;
    assert_eq!(counts, "3,2,5,2,1,2,2,1,3");

    let user = payload(&store, "user", tape::PICARD).await.unwrap();
    assert_eq!(user["email"], "picard@enterprise.test");
    for secret in ["token", "websocket_url"] {
        assert!(
            user.get(secret).is_none(),
            "the account's {secret} was stored"
        );
    }
    assert!(user.get("karma").is_none(), "karma is volatile");

    let coils = payload(&store, "items", tape::COILS).await.unwrap();
    assert_eq!(coils["labels"], serde_json::json!(["engineering", "warp"]));
    assert!(coils.get("day_order").is_none(), "day_order is volatile");

    assert_eq!(
        scalar(&store, "SELECT sync_token FROM sync_state").await,
        "tok-2"
    );
    assert_eq!(
        scalar(&store, "SELECT lo || ' ' || hi FROM coverage").await,
        "2026-03-02T09:00:00Z 2026-10-01T00:00:00Z",
        "the whole range read, as one span"
    );
}

/// A week later: the sync sends only what changed, and the walk of
/// completed tasks reads only the new week.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_later_sync_reads_only_what_changed() {
    let d = tempfile::tempdir().unwrap();
    let store = d.path().join("store");
    run(
        &store,
        &tape::tape(d.path(), Edition::Before),
        Edition::Before,
    )
    .await
    .unwrap();
    let s = run(
        &store,
        &tape::tape(d.path(), Edition::After),
        Edition::After,
    )
    .await
    .unwrap();
    assert_eq!(s.syncs, 1, "{s:?}");
    assert!(!s.full_sync);
    assert_eq!(
        s.synced, 6,
        "the account, a project, three tasks, a comment: {s:?}"
    );
    assert_eq!(s.requests, 2, "{s:?}");
    assert_eq!(s.completed_written, 1, "{s:?}");

    let coils = payload(&store, "items", tape::COILS).await.unwrap();
    assert_eq!(coils["checked"], true);
    let farpoint = payload(&store, "items", tape::FARPOINT).await.unwrap();
    assert_eq!(
        farpoint["is_deleted"], true,
        "a deleted task is kept as Todoist said"
    );
    assert!(payload(&store, "items", tape::BRIEFING).await.is_some());
    assert!(payload(&store, "completed_items", tape::COILS)
        .await
        .is_some());
    assert_eq!(
        payload(&store, "projects", tape::AWAY).await.unwrap()["name"],
        "Away missions, stardate 47457"
    );
    assert_eq!(
        scalar(&store, "SELECT sync_token FROM sync_state").await,
        "tok-3"
    );
    assert_eq!(
        scalar(&store, "SELECT lo || ' ' || hi FROM coverage").await,
        "2026-03-02T09:00:00Z 2026-10-08T00:00:00Z"
    );
}

/// A run with nothing new upstream asks one sync and stores nothing:
/// the account it sends again, unchanged but for its karma, is not a
/// change.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_with_nothing_new_writes_nothing() {
    let d = tempfile::tempdir().unwrap();
    let t = tape::tape(d.path(), Edition::Before);
    let store = d.path().join("store");
    run(&store, &t, Edition::Before).await.unwrap();
    let fetched = || {
        scalar(
            &store,
            "SELECT group_concat(id || '@' || fetched_at_utc) FROM user_bookkeeping",
        )
    };
    let before = fetched().await;
    let s = run(&store, &t, Edition::Before).await.unwrap();
    assert_eq!(s.requests, 1, "{s:?}");
    assert_eq!(s.completed_written, 0, "{s:?}");
    assert_eq!(fetched().await, before);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_tasks_are_left_alone_when_turned_off() {
    let d = tempfile::tempdir().unwrap();
    let t = tape::tape(d.path(), Edition::Before);
    let store = d.path().join("store");
    let s = run_with(
        &store,
        &t,
        Edition::Before,
        TodoistSync {
            completed: false,
            completed_since: None,
        },
    )
    .await
    .unwrap();
    assert_eq!(s.requests, 2, "{s:?}");
    assert_eq!(
        scalar(&store, "SELECT CAST(COUNT(*) AS TEXT) FROM completed_items").await,
        "0"
    );
}

/// `completed_since` stops the walk at its date rather than at the day
/// the account joined: of the tape's windows, only the newest is asked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn completed_since_bounds_the_walk() {
    let d = tempfile::tempdir().unwrap();
    let t = tape::tape(d.path(), Edition::Before);
    let windows = tape::windows_of(Edition::Before, &[]);
    let store = d.path().join("store");
    let s = run_with(
        &store,
        &t,
        Edition::Before,
        TodoistSync {
            completed: true,
            completed_since: Some(windows[0].0[..10].to_string()),
        },
    )
    .await
    .unwrap();
    assert_eq!(s.windows, 1, "{s:?}");
    assert_eq!(s.completed_written, 1, "{s:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_credential_fails_the_run_and_says_how_to_sign_in() {
    let d = tempfile::tempdir().unwrap();
    let t = tape::refused(d.path());
    let e = run(&d.path().join("store"), &t, Edition::Before)
        .await
        .unwrap_err();
    let said = format!("{e:#}");
    assert!(said.contains("AUTH_INVALID_TOKEN"), "{said}");
    assert!(said.contains("latchkey auth browser todoist"), "{said}");
}
