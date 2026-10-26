//! A label filter keeps whole threads (#1368): the rest of a kept
//! message's thread, which a walk of the whole account names, is listed
//! and fetched whatever its labels, and a thread no kept message is in
//! any more goes. With `whole_threads = false` only the matching
//! messages are kept, and the thread row says how many Gmail has. Each
//! email carries Gmail's ids and labels (#1370).
//!
//! Driven through the HTTP playback layer: no credential, no network.

use std::collections::BTreeSet;
use std::path::Path;

use datalib_etl_email::ingest::gmail_api::{self, FetchOptions, FetchSummary};
use serde_json::{json, Value};

use crate::support::{
    gmail_get_url, gmail_history_url, gmail_list_url, gmail_message, inbox_label, put_gmail,
    put_gmail_account, put_gmail_all_mail, Mirror,
};

/// Archived out of the inbox, under `away team`: the thread's start.
const HAIL: &str = "18c9f2a1b2c3d801";
/// The reply, still in the inbox.
const REPLY: &str = "18c9f2a1b2c3d802";
const AWAY_TEAM: &str = "Label_7";

/// The first run keeps the reply its walk of the inbox listed, then
/// fetches the hail its thread names, though the hail is not in the
/// inbox; the thread holds what Gmail has.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_rest_of_a_kept_messages_thread_is_mirrored() {
    let h = Harness::new();
    let run = h.run(None).await;
    assert_eq!(run.walked, ["INBOX"], "{run:?}");
    assert_eq!(run.listed_from_threads, 1, "{run:?}");
    assert_eq!(run.emails_upserted, 2, "{run:?}");
    assert_eq!(h.0.gmail_ids().await, ids(&[HAIL, REPLY]));
    assert_eq!(h.thread_counts().await, [(Some(2), Some(2))]);

    let again = h.run(None).await;
    assert!(again.walked.is_empty(), "{again:?}");
    assert_eq!(again.emails_upserted, 0, "{again:?}");
    assert_eq!(again.quota_units_spent, 4, "{again:?}");
}

/// Kept by label alone, the thread is held in part, and its row says so.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_whole_threads_a_partial_thread_shows_gmails_count() {
    let h = Harness::new();
    let run = h.run(Some(false)).await;
    assert_eq!(run.listed_from_threads, 0, "{run:?}");
    assert_eq!(h.0.gmail_ids().await, ids(&[REPLY]));
    assert_eq!(h.thread_counts().await, [(Some(1), Some(2))]);
}

/// A reply the filter drops is still a message of the thread: the
/// count goes up with it, from what `history.list` said, and nothing is
/// asked of Gmail about the thread.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_the_filter_drops_still_counts_toward_its_thread() {
    const SENT: &str = "18c9f2a1b2c3d803";
    let h = Harness::new();
    h.run(Some(false)).await;

    let p = &h.0.playback;
    put_gmail_account(p, "1001", labels());
    put_gmail(
        p,
        &gmail_history_url("1000"),
        &json!({
            "history": [{ "messagesAdded": [
                { "message": { "id": SENT, "threadId": HAIL, "labelIds": ["SENT"] } },
            ]}],
            "historyId": "1001",
        }),
    );
    put_gmail(p, &gmail_get_url(SENT), &message(SENT, &["SENT"]));
    let run = h.run(Some(false)).await;
    assert_eq!(run.messages_filtered, 1, "{run:?}");
    assert_eq!(run.threads_recounted, 1, "{run:?}");
    assert_eq!(h.0.gmail_ids().await, ids(&[REPLY]));
    assert_eq!(h.thread_counts().await, [(Some(1), Some(3))]);
}

/// The reply is archived too: no message of the thread is in the inbox,
/// so the whole thread goes, the hail with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_thread_the_filter_no_longer_keeps_goes_whole() {
    let h = Harness::new();
    h.run(None).await;

    let p = &h.0.playback;
    put_gmail_account(p, "1001", labels());
    put_gmail(
        p,
        &gmail_history_url("1000"),
        &json!({
            "history": [{ "labelsRemoved": [
                { "message": { "id": REPLY, "threadId": HAIL } },
            ]}],
            "historyId": "1001",
        }),
    );
    // The reply's bytes are held, so only its labels are asked for: by
    // walking every label, which costs less than one message at a time.
    put_gmail(p, &gmail_list_url(&["INBOX"]), &json!({}));
    put_gmail(
        p,
        &gmail_list_url(&[AWAY_TEAM]),
        &json!({ "messages": [{ "id": HAIL }] }),
    );
    let run = h.run(None).await;
    assert_eq!(run.emails_destroyed, 2, "{run:?}");
    assert!(h.0.gmail_ids().await.is_empty());
    assert!(h.thread_counts().await.is_empty());
}

/// Gmail's ids in the API's hex, and its labels as it spells them, with
/// `UNREAD` and `STARRED` back among them.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_email_carries_gmails_ids_and_labels() {
    let h = Harness::new();
    put_gmail(
        &h.0.playback,
        &gmail_get_url(REPLY),
        &message(REPLY, &["INBOX", "UNREAD", "STARRED"]),
    );
    h.run(None).await;
    let rows: Vec<(String, String, String, String)> =
        h.0.read(|db| async move {
            sqlx::query_as(
                "SELECT gmail_message_id, gmail_thread_id, label_ids, thread_id
                 FROM emails ORDER BY id",
            )
            .fetch_all(db.pool())
            .await
            .unwrap()
        })
        .await;
    // The decimal a Takeout writes, which `thread_id` keeps.
    let thread = u64::from_str_radix(HAIL, 16).unwrap().to_string();
    assert_eq!(
        rows,
        [
            (
                HAIL.to_string(),
                HAIL.to_string(),
                r#"["Label_7"]"#.to_string(),
                thread.clone(),
            ),
            (
                REPLY.to_string(),
                HAIL.to_string(),
                r#"["INBOX","STARRED","UNREAD"]"#.to_string(),
                thread,
            ),
        ]
    );
}

fn ids(v: &[&str]) -> BTreeSet<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn labels() -> Value {
    json!([inbox_label(), { "id": AWAY_TEAM, "name": "away team", "type": "user" }])
}

/// A message under `label_ids` in the hail's thread.
fn message(id: &str, label_ids: &[&str]) -> Value {
    let mut m = gmail_message(id, label_ids, "Hailing frequencies");
    m["threadId"] = json!(HAIL);
    m
}

struct Harness(Mirror);

impl Harness {
    fn new() -> Self {
        let m = Mirror::new();
        write_fixtures(&m.playback);
        Self(m)
    }

    async fn run(&self, whole_threads: Option<bool>) -> FetchSummary {
        self.0
            .run(|db| {
                let mut opts = FetchOptions::new(db);
                opts.only_labels = vec!["Inbox".to_string()];
                opts.whole_threads = whole_threads.unwrap_or(true);
                gmail_api::fetch(opts)
            })
            .await
            .expect("gmail fetch under playback")
    }

    /// Each thread's `(email_count, upstream_email_count)`.
    async fn thread_counts(&self) -> Vec<(Option<i64>, Option<i64>)> {
        self.0
            .read(|db| async move {
                sqlx::query_as("SELECT email_count, upstream_email_count FROM threads")
                    .fetch_all(db.pool())
                    .await
                    .unwrap()
            })
            .await
    }
}

fn write_fixtures(playback: &Path) {
    put_gmail_account(playback, "1000", labels());
    put_gmail(
        playback,
        &gmail_history_url("1000"),
        &json!({ "historyId": "1000" }),
    );
    put_gmail(
        playback,
        &gmail_list_url(&["INBOX"]),
        &json!({ "messages": [{ "id": REPLY, "threadId": HAIL }] }),
    );
    let messages = [message(HAIL, &[AWAY_TEAM]), message(REPLY, &["INBOX"])];
    for m in &messages {
        put_gmail(playback, &gmail_get_url(m["id"].as_str().unwrap()), m);
    }
    put_gmail_all_mail(playback, &messages);
}
