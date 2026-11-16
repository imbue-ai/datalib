//! The Baby Connect download driven end to end through HTTP playback, on
//! the TNG tapes in [`crate::tape`].

use std::path::Path;

use datalib_etl::doltlite_raw::commit_run;
use datalib_etl::upstream::Upstream;
use datalib_etl_baby_connect::ingest::{self, db_path_for, FetchSummary, RawDb};
use datalib_etl_baby_connect_config::BabyConnectSync;
use datalib_etl_web::playback::{self, Asked, Playback};
use serde_json::Value;

use crate::tape::{self, Edition};

async fn run(
    store: &Path,
    tape: &Path,
    edition: Edition,
    sync: BabyConnectSync,
) -> (anyhow::Result<FetchSummary>, Asked) {
    let asked = Asked::default();
    let db = RawDb::open(&db_path_for(store)).await.unwrap();
    let r = playback::scope(
        Playback::at(tape).asked(asked.clone()),
        ingest::fetch(ingest::FetchOptions {
            sync,
            now: Some(edition.now()),
            ..ingest::FetchOptions::new(db.clone())
        }),
    )
    .await;
    commit_run(db.pool(), "test").await.unwrap();
    db.close().await;
    (r, asked)
}

async fn rows(store: &Path, sql: &'static str) -> Vec<(String, String)> {
    let db = RawDb::open(&db_path_for(store)).await.unwrap();
    let v: Vec<(String, String)> = sqlx::query_as(sql).fetch_all(db.pool()).await.unwrap();
    db.close().await;
    v
}

async fn entry(store: &Path, id: u64) -> Option<Value> {
    let db = RawDb::open(&db_path_for(store)).await.unwrap();
    let p: Option<Option<String>> =
        sqlx::query_scalar("SELECT json(payload) FROM entries WHERE id = ?")
            .bind(id.to_string())
            .fetch_optional(db.pool())
            .await
            .unwrap();
    db.close().await;
    p.flatten().map(|p| serde_json::from_str(&p).unwrap())
}

/// The first run reads every day from the eldest child's birthday to
/// today, keeps only each day's own entries, and records as read only
/// the days more than a day old.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_first_run_reads_every_day_since_the_eldest_birthday() {
    let (store, before) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    tape::write(before.path(), Edition::Before);
    let (r, asked) = run(
        store.path(),
        before.path(),
        Edition::Before,
        BabyConnectSync::default(),
    )
    .await;
    let s = r.unwrap();
    assert_eq!((s.kids, s.days, s.days_settled), (1, 5, 3), "{}", s.line());
    assert_eq!(asked.count("StatusList"), 5);
    let ids: Vec<(String, String)> =
        rows(store.path(), "SELECT id, '' FROM kids ORDER BY id").await;
    assert_eq!(
        ids.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
        vec![tape::WESLEY.to_string()]
    );
    let coverage = rows(
        store.path(),
        "SELECT lo, hi FROM coverage WHERE scope = 'days'",
    )
    .await;
    assert_eq!(
        coverage,
        vec![("2048-10-12".to_string(), "2048-10-14".to_string())]
    );
    let users = rows(store.path(), "SELECT id, '' FROM users ORDER BY id").await;
    assert_eq!(users.len(), 2);

    let spilled = entry(store.path(), tape::SPILLED).await.unwrap();
    assert_eq!(spilled["Pdt"], 481013, "kept from its own day's answer");
    let diary = rows(
        store.path(),
        "SELECT id, json_extract(payload, '$.n') FROM entries WHERE json_extract(payload, '$.Cat') = 2600",
    )
    .await;
    assert_eq!(diary.len(), 1);
    assert!(diary[0].1.contains("would not let go"), "{diary:?}");
    let all = rows(store.path(), "SELECT id, '' FROM entries").await;
    assert_eq!(
        all.len(),
        s.entries,
        "an entry spilled into a neighbour's answer is stored once"
    );
}

/// A second run asks only for the days the first left unsettled, and an
/// entry edited on one of them is stored at its new version.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn later_runs_read_only_the_days_not_yet_settled() {
    let store = tempfile::tempdir().unwrap();
    let (before, after) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    tape::write(before.path(), Edition::Before);
    tape::write(after.path(), Edition::After);
    let sync = BabyConnectSync::default();
    run(store.path(), before.path(), Edition::Before, sync.clone())
        .await
        .0
        .unwrap();
    let (again, asked) = run(store.path(), before.path(), Edition::Before, sync.clone()).await;
    assert_eq!(again.unwrap().days, 2);
    assert_eq!(asked.count("StatusList"), 2, "only the 15th and 16th");

    let (later, asked) = run(store.path(), after.path(), Edition::After, sync).await;
    let later = later.unwrap();
    assert_eq!((later.days, later.days_settled), (4, 2), "{}", later.line());
    assert_eq!(asked.count("StatusList"), 4);
    let edited = entry(store.path(), tape::EDITED).await.unwrap();
    assert_eq!(edited["Txt"], "Wesley slept 3 hrs");
    assert_eq!(edited["n"], "Fixed: it was three hours, not two.");
    assert!(
        edited.get("readby").is_none(),
        "who has read it is volatile"
    );
    let coverage = rows(
        store.path(),
        "SELECT lo, hi FROM coverage WHERE scope = 'days'",
    )
    .await;
    assert_eq!(
        coverage,
        vec![("2048-10-12".to_string(), "2048-10-16".to_string())]
    );
}

/// `since` and `until` bound the days read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_window_bounds_the_days_read() {
    let (store, before) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    tape::write(before.path(), Edition::Before);
    let sync = BabyConnectSync {
        since: Some("2048-10-13".to_string()),
        until: Some("2048-10-14".to_string()),
    };
    let (r, asked) = run(store.path(), before.path(), Edition::Before, sync).await;
    assert_eq!(r.unwrap().days, 2);
    assert_eq!(asked.count("StatusList"), 2);
}

/// With more than one child on the account, a day is asked for every
/// child at once, and each entry is filed under its own child.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_nursery_account_asks_for_every_child_at_once() {
    let (store, tape_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    tape::write_nursery(tape_dir.path());
    let sync = BabyConnectSync {
        since: Some(tape::NURSERY_DAY.to_string()),
        until: Some(tape::NURSERY_DAY.to_string()),
    };
    let (r, asked) = run(store.path(), tape_dir.path(), Edition::Nursery, sync).await;
    let s = r.unwrap();
    assert_eq!((s.kids, s.days, s.entries), (2, 1, 2), "{}", s.line());
    assert_eq!(asked.count("StatusList"), 1);
    let kids = rows(
        store.path(),
        "SELECT DISTINCT CAST(json_extract(payload, '$.Kid') AS TEXT), '' FROM entries",
    )
    .await;
    assert_eq!(kids, vec![(tape::ALEXANDER.to_string(), String::new())]);
}

/// A signed-out session is shown the login page in place of the home
/// page; the run fails as a refused credential, with the way to sign in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_signed_out_session_fails_as_a_refused_credential() {
    let (store, tape_dir) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    tape::write_signed_out(tape_dir.path());
    let (r, asked) = run(
        store.path(),
        tape_dir.path(),
        Edition::Before,
        BabyConnectSync::default(),
    )
    .await;
    let e = r.unwrap_err();
    assert_eq!(
        Upstream::in_chain(&e),
        Some(Upstream::CredentialRefused),
        "{e:#}"
    );
    assert!(
        format!("{e:#}").contains("latchkey auth browser babyconnect"),
        "{e:#}"
    );
    assert_eq!(asked.count("StatusList"), 0);
}
