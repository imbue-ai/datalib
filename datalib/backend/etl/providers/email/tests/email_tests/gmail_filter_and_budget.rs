//! `only_extract_labels` spelled with Gmail's label ids, `message_budget`,
//! and a store whose label filter once threw away what a walk listed
//! (#1360, #1361, #1362, #1365).
//!
//! Driven through the HTTP playback layer: no credential, no network.

use std::collections::BTreeSet;
use std::path::Path;

use datalib_etl_email::ingest::gmail_api::{self, FetchOptions, FetchSummary};
use serde_json::json;

use crate::support::{
    gmail_get_url, gmail_history_url, gmail_list_url, gmail_message, inbox_label, put_gmail,
    put_gmail_account, Mirror,
};

const OLDER: &str = "18c9f2a1b2c3d601";
const NEWER: &str = "18c9f2a1b2c3d602";
const SENT: &str = "18c9f2a1b2c3d603";

/// `["INBOX", "SENT"]` narrowed the walks to those labels and then
/// dropped every message they listed, because the per-message check
/// compared the ids with the labels' display names (`Inbox`, `Sent`).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_filter_of_label_ids_keeps_the_mail_its_walks_list() {
    let h = Harness::new();
    let run = h.run(&["INBOX", "SENT"], None).await;
    assert_eq!(run.messages_filtered, 0, "{run:?}");
    assert_eq!(run.emails_upserted, 3, "{run:?}");
    assert_eq!(h.mirrored().await, ids(&[OLDER, NEWER, SENT]));
}

/// The names, and a mix of both spellings, keep the same mail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_filter_of_label_names_keeps_the_same_mail() {
    for labels in [&["Inbox", "Sent"][..], &["INBOX", "Sent"][..]] {
        let h = Harness::new();
        let run = h.run(labels, None).await;
        assert_eq!(run.messages_filtered, 0, "{labels:?}: {run:?}");
        assert_eq!(h.mirrored().await, ids(&[OLDER, NEWER, SENT]), "{labels:?}");
    }
}

/// A budget fetched nothing: the run set "stopped at the budget" before
/// the fetch began, and the fetch read that as a reason not to start.
/// Every run after did the same.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_budget_fetches_that_many_and_the_next_run_goes_on() {
    let h = Harness::new();
    let first = h.run(&[], Some(2)).await;
    assert!(first.budget_exhausted, "{first:?}");
    assert_eq!(first.emails_upserted, 2, "{first:?}");
    assert_eq!(h.mirrored().await.len(), 2);

    let second = h.run(&[], Some(2)).await;
    assert!(!second.budget_exhausted, "{second:?}");
    assert_eq!(second.emails_upserted, 1, "{second:?}");
    assert_eq!(h.mirrored().await, ids(&[OLDER, NEWER, SENT]));
}

/// The account row carries the address `users.getProfile` reported, in
/// its column as well as its payload.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_account_row_carries_the_profiles_address() {
    let h = Harness::new();
    h.run(&[], None).await;
    let address: Option<String> =
        h.0.read(|db| async move {
            sqlx::query_scalar("SELECT email_address FROM accounts")
                .fetch_one(db.pool())
                .await
                .unwrap()
        })
        .await;
    assert_eq!(address.as_deref(), Some("t@example.test"));
}

/// A message a walk of an admitted label listed, then dropped by the
/// filter, lost its listing; the label stayed recorded as listed whole,
/// so no later run walked it again and the message was never owed
/// again. The rung that clears those records lets the next run list it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_the_filter_dropped_is_listed_again_after_the_upgrade() {
    let h = Harness::new();
    // What the filter saw: the newest message without the label its walk
    // listed it under, as a build that misread the filter saw them all.
    put_gmail(
        &h.0.playback,
        &gmail_get_url(NEWER),
        &gmail_message(NEWER, &["CATEGORY_UPDATES"], NEWER),
    );
    let dropped = h.run(&["INBOX"], None).await;
    assert_eq!(dropped.messages_filtered, 1, "{dropped:?}");
    put_gmail(
        &h.0.playback,
        &gmail_get_url(NEWER),
        &gmail_message(NEWER, &["INBOX"], NEWER),
    );

    let unchanged = h.run(&["INBOX"], None).await;
    assert!(unchanged.walked.is_empty(), "{unchanged:?}");
    assert_eq!(
        h.mirrored().await,
        ids(&[OLDER]),
        "the bug: never owed again"
    );

    // A store that build left is one rung below this one's top.
    h.0.read(|db| async move {
        sqlx::query("UPDATE _datalib_meta SET value = '4' WHERE key = 'schema_version'")
            .execute(db.pool())
            .await
            .unwrap();
    })
    .await;
    let upgraded = h.run(&["INBOX"], None).await;
    assert_eq!(upgraded.walked, vec!["INBOX".to_string()], "{upgraded:?}");
    assert_eq!(upgraded.emails_upserted, 1, "{upgraded:?}");
    assert_eq!(h.mirrored().await, ids(&[OLDER, NEWER]));
}

fn ids(v: &[&str]) -> BTreeSet<String> {
    v.iter().map(|s| s.to_string()).collect()
}

struct Harness(Mirror);

impl Harness {
    fn new() -> Self {
        let m = Mirror::new();
        write_fixtures(&m.playback);
        Self(m)
    }

    async fn run(&self, labels: &[&str], budget: Option<usize>) -> FetchSummary {
        self.0
            .run(|db| {
                let mut opts = FetchOptions::new(db);
                opts.only_labels = labels.iter().map(|s| s.to_string()).collect();
                opts.config.message_budget = budget;
                gmail_api::fetch(opts)
            })
            .await
            .expect("gmail fetch under playback")
    }

    async fn mirrored(&self) -> BTreeSet<String> {
        self.0.gmail_ids().await
    }
}

fn write_fixtures(playback: &Path) {
    put_gmail_account(
        playback,
        "1000",
        json!([
            inbox_label(),
            { "id": "SENT", "name": "SENT", "type": "system" },
            { "id": "CATEGORY_UPDATES", "name": "CATEGORY_UPDATES", "type": "system" },
        ]),
    );
    put_gmail(
        playback,
        &gmail_history_url("1000"),
        &json!({ "historyId": "1000" }),
    );
    put_gmail(
        playback,
        &gmail_list_url(&["INBOX"]),
        &json!({ "messages": [{ "id": NEWER }, { "id": OLDER }] }),
    );
    put_gmail(
        playback,
        &gmail_list_url(&["SENT"]),
        &json!({ "messages": [{ "id": SENT }] }),
    );
    put_gmail(
        playback,
        &gmail_list_url(&[]),
        &json!({ "messages": [{ "id": SENT }, { "id": NEWER }, { "id": OLDER }] }),
    );
    for (id, labels) in [
        (OLDER, &["INBOX"][..]),
        (NEWER, &["INBOX", "UNREAD"][..]),
        (SENT, &["SENT"][..]),
    ] {
        put_gmail(playback, &gmail_get_url(id), &gmail_message(id, labels, id));
    }
}
