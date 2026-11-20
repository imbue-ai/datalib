//! The Gmail download against the fake Google's Gmail.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_email::ingest::gmail_api::{self, FetchOptions, FetchSummary, Section};
use datalib_etl_email::ingest::{db_path_for, RawDb};
use datalib_fake_services::{with_real_rate_limits, Fakes, Launch, GOOGLE};
use datalib_time::IsoOffsetTimestamp;
use serde_json::{json, Value};

const TOKEN: &str = "ya29.fake-starfleet-google-token";
const PICARD: &str = "picard@enterprise.example";

/// Picard's mailbox: a two-message thread from Starfleet Command, one
/// unread, one archived newsletter, and a label of his own.
fn seed() -> Value {
    json!({
        "anchor": "2026-03-21T00:00:00Z",
        "time_shift": "none",
        "rate_limits": "none",
        "tokens": [{"token": TOKEN, "user": PICARD}],
        "users": [
            {"email": PICARD, "name": "Jean-Luc Picard"},
            {"email": "riker@enterprise.example", "name": "William Riker"},
        ],
        "labels": [{"mailbox": PICARD, "name": "Command"}],
        "messages": [
            {"key": "orders", "mailbox": PICARD, "from": "Admiral Nechayev <nechayev@starfleet.example>", "to": PICARD, "subject": "Orders: proceed to Farpoint", "date": "2026-03-16T09:15:00-07:00", "body": "Proceed to Farpoint Station at your earliest convenience.", "labels": ["Command", "IMPORTANT"], "message_id": "<orders-1@starfleet.example>"},
            {"key": "orders-reply", "mailbox": PICARD, "from": "Jean-Luc Picard <picard@enterprise.example>", "to": "Admiral Nechayev <nechayev@starfleet.example>", "subject": "Re: Orders: proceed to Farpoint", "date": "2026-03-16T17:40:00+01:00", "body": "Acknowledged. Course laid in.", "in_reply_to": "orders"},
            {"key": "orders-followup", "mailbox": PICARD, "from": "Admiral Nechayev <nechayev@starfleet.example>", "to": PICARD, "subject": "Re: Orders: proceed to Farpoint", "date": "2026-03-19T08:05:00-07:00", "body": "Report on arrival.", "labels": ["Command", "UNREAD"], "in_reply_to": "orders-reply"},
            {"key": "newsletter", "mailbox": PICARD, "from": "Starfleet Weekly <news@starfleet.example>", "to": PICARD, "subject": "This week in the fleet", "date": "2026-03-12T06:00:00Z", "body": "Ten new ships commissioned.", "archived": true},
        ]
    })
}

async fn fetch_into(
    root: &Path,
    adjust: impl FnOnce(FetchOptions) -> FetchOptions,
) -> anyhow::Result<FetchSummary> {
    let db = RawDb::open(&db_path_for(root)).await?;
    let summary = gmail_api::fetch(adjust(FetchOptions::new(db.clone()))).await;
    if summary.is_ok() {
        db.commit_all("test").await?;
    }
    db.close().await;
    summary
}

async fn rows(root: &Path, sql: &'static str) -> Vec<(String, Option<String>)> {
    let db = RawDb::open(&db_path_for(root)).await.unwrap();
    let rows: Vec<(String, Option<String>)> =
        sqlx::query_as(sql).fetch_all(db.pool()).await.unwrap();
    db.close().await;
    rows
}

async fn subjects(root: &Path) -> BTreeMap<String, Option<String>> {
    rows(root, "SELECT id, subject FROM emails ORDER BY id")
        .await
        .into_iter()
        .collect()
}

/// The ids of the emails not yet read.
async fn unread(root: &Path) -> BTreeSet<String> {
    rows(
        root,
        "SELECT id, CAST(read AS TEXT) FROM read_marks ORDER BY id",
    )
    .await
    .into_iter()
    .filter(|(_, read)| read.as_deref() == Some("0"))
    .map(|(id, _)| id)
    .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_first_sync_mirrors_the_mailbox_with_its_labels_threads_and_read_marks() {
    let _fakes = Fakes::start(&BTreeMap::from([(GOOGLE, seed())])).expect("the fakes serve");
    let root = tempfile::tempdir().unwrap();

    let summary = fetch_into(root.path(), |o| o)
        .await
        .expect("the download runs");

    let shown = serde_json::to_string(&summary).unwrap();
    assert_eq!(summary.emails_upserted, 4, "{shown}");
    let subjects = subjects(root.path()).await;
    assert_eq!(subjects.len(), 4, "{subjects:?}");
    let by_subject: BTreeSet<&str> = subjects.values().flatten().map(String::as_str).collect();
    assert_eq!(
        by_subject,
        [
            "Orders: proceed to Farpoint",
            "Re: Orders: proceed to Farpoint",
            "This week in the fleet"
        ]
        .into()
    );
    // The two replies share the first message's thread.
    let threads = rows(root.path(), "SELECT id, thread_id FROM emails ORDER BY id").await;
    let thread_ids: BTreeSet<Option<String>> = threads.into_iter().map(|(_, t)| t).collect();
    assert_eq!(thread_ids.len(), 2, "{thread_ids:?}");
    assert_eq!(unread(root.path()).await.len(), 1);
    // Picard's own label reaches the mirror as a mailbox.
    let mailboxes = rows(root.path(), "SELECT id, name FROM mailboxes ORDER BY id").await;
    assert!(
        mailboxes
            .iter()
            .any(|(_, name)| name.as_deref() == Some("Command")),
        "{mailboxes:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_second_sync_reads_the_history_for_a_new_message_and_a_read_mark() {
    let fakes = Fakes::start(&BTreeMap::from([(GOOGLE, seed())])).expect("the fakes serve");
    let root = tempfile::tempdir().unwrap();
    fakes.mark_phase("first").unwrap();
    fetch_into(root.path(), |o| o)
        .await
        .expect("the first download runs");
    let unread_before = unread(root.path()).await;
    fakes
        .add_records(
            GOOGLE,
            &json!({
                "messages": [
                    {"key": "arrival", "mailbox": PICARD, "from": "William Riker <riker@enterprise.example>", "to": PICARD, "subject": "Farpoint in range", "date": "2026-03-20T15:30:00-07:00", "body": "Dropping out of warp in ten minutes.", "labels": ["UNREAD"]},
                ],
                "label_changes": [
                    {"message": "orders-followup", "add": ["Command"], "remove": ["UNREAD"]},
                ]
            }),
            None,
        )
        .expect("the update lands");

    fakes.mark_phase("second").unwrap();
    let second = fetch_into(root.path(), |o| o)
        .await
        .expect("the second download runs");

    // Two rows written: the arrival, whose body is the one blob fetched,
    // and the follow-up, whose labels moved.
    let shown = serde_json::to_string(&second).unwrap();
    assert_eq!(second.emails_upserted, 2, "{shown}");
    assert_eq!(second.blobs_stored, 1, "{shown}");
    assert_eq!(second.relabeled_one_by_one, 1, "{shown}");
    assert!(
        subjects(root.path())
            .await
            .values()
            .flatten()
            .any(|s| s == "Farpoint in range"),
        "{shown}"
    );
    let unread_after = unread(root.path()).await;
    assert_eq!(unread_before.len(), 1);
    assert_eq!(
        unread_after.len(),
        1,
        "the follow-up was read and the arrival is not: {unread_after:?}"
    );
    assert!(
        unread_before.is_disjoint(&unread_after),
        "{unread_before:?} {unread_after:?}"
    );
    let history_calls = fakes
        .requests_in_phase("second")
        .iter()
        .filter(|line| line["url"].as_str().unwrap().contains("/history"))
        .count();
    assert!(
        history_calls >= 1,
        "{:?}",
        fakes.requests_in_phase("second")
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_window_lists_only_the_mail_after_its_start() {
    let _fakes = Fakes::start(&BTreeMap::from([(GOOGLE, seed())])).expect("the fakes serve");
    let root = tempfile::tempdir().unwrap();
    let since: IsoOffsetTimestamp = "2026-03-15T00:00:00+00:00".parse().unwrap();

    let summary = fetch_into(root.path(), |o| FetchOptions {
        sections: vec![Section {
            since: Some(since),
            whole_threads: false,
            ..Section::labels(Vec::new())
        }],
        ..o
    })
    .await
    .expect("the download runs");

    assert_eq!(
        summary.emails_upserted,
        3,
        "{}",
        serde_json::to_string(&summary).unwrap()
    );
    assert!(!subjects(root.path())
        .await
        .values()
        .flatten()
        .any(|s| s == "This week in the fleet"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_sync_past_gmails_per_user_quota_waits_and_completes() {
    // A full `messages.get` costs 60 units, so a bucket of 150 a second
    // takes two and refuses the third with a Retry-After; a bucket a single
    // request overruns would be refused for good, as Gmail refuses a
    // request past its per-request maximum.
    let launch = Launch {
        rate_limit_by_bucket: BTreeMap::from([("google:gmail_per_user".to_string(), 150)]),
        rate_limit_window_seconds: Some(1),
    };
    let fakes = Fakes::start_with(
        &BTreeMap::from([(GOOGLE, with_real_rate_limits(seed()))]),
        &launch,
    )
    .expect("the fakes serve");
    let root = tempfile::tempdir().unwrap();

    let summary = fetch_into(root.path(), |o| o)
        .await
        .expect("the download runs");

    assert_eq!(
        summary.emails_upserted,
        4,
        "{}",
        serde_json::to_string(&summary).unwrap()
    );
    let refused = fakes.health()["rate_limited"]["google:gmail_per_user"]
        .as_u64()
        .unwrap_or(0);
    assert!(refused >= 1, "{}", fakes.health());
}
