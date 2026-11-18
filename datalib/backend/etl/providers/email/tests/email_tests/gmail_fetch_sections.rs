//! `gmail.fetch`: sections that add up, each with its own window. The case
//! they exist for is all of the inbox, because old unanswered mail matters,
//! and only recent sent mail, which only says who the account has written to.
//!
//! Each run is the real `datalib-step` ingest of a source configured as a
//! person would configure it, answered from playback fixtures: no credential,
//! no network.

use std::collections::BTreeSet;

use serde_json::{json, Value};

use crate::support::{
    gmail_get_url, gmail_history_url, gmail_list_url, gmail_message, inbox_label, put_gmail,
    put_gmail_account, put_gmail_all_mail, Mirror,
};

const SEPTEMBER: &str = "2026-09-01T00:00:00+00:00";
const AUGUST: &str = "2026-08-01T00:00:00+00:00";

const OLD_IN_INBOX: &str = "18c9f2a1b2c3d701";
const RECENT_SENT: &str = "18c9f2a1b2c3d702";
const OLD_SENT: &str = "18c9f2a1b2c3d703";
/// What `OLD_SENT` answered, in no label the sections name.
const REPLY_TO_OLD_SENT: &str = "18c9f2a1b2c3d704";
/// In the thread of `RECENT_SENT`, in no label the sections name.
const START_OF_RECENT_SENT: &str = "18c9f2a1b2c3d705";

fn labels() -> Value {
    json!([inbox_label(), { "id": "SENT", "name": "SENT", "type": "system" }])
}

fn secs(since: &str) -> i64 {
    datalib_time::parse_strict(since)
        .unwrap()
        .inner()
        .timestamp()
}

fn message(id: &str, thread: &str, label_ids: &[&str]) -> Value {
    let mut message = gmail_message(id, label_ids, id);
    message["threadId"] = json!(thread);
    message
}

fn listed(messages: &[&Value]) -> Value {
    let page: Vec<Value> = messages
        .iter()
        .map(|m| json!({ "id": m["id"], "threadId": m["threadId"] }))
        .collect();
    json!({ "messages": page })
}

fn sent_url(since: Option<&str>) -> String {
    let url = gmail_list_url(&["SENT"]);
    match since {
        Some(since) => format!("{url}&q=after%3A{}", secs(since)),
        None => url,
    }
}

/// An account with old mail in the inbox and sent mail on either side of
/// September.
fn account() -> Mirror {
    let m = Mirror::new();
    put_gmail_account(&m.playback, "1000", labels());
    put_gmail(
        &m.playback,
        &gmail_history_url("1000"),
        &json!({ "historyId": "1000" }),
    );
    let inbox = message(OLD_IN_INBOX, OLD_IN_INBOX, &["INBOX"]);
    let recent = message(RECENT_SENT, START_OF_RECENT_SENT, &["SENT"]);
    let start = message(START_OF_RECENT_SENT, START_OF_RECENT_SENT, &[]);
    let old = message(OLD_SENT, OLD_SENT, &["SENT"]);
    let reply = message(REPLY_TO_OLD_SENT, OLD_SENT, &[]);
    put_gmail(&m.playback, &gmail_list_url(&["INBOX"]), &listed(&[&inbox]));
    put_gmail(&m.playback, &sent_url(Some(SEPTEMBER)), &listed(&[&recent]));
    put_gmail(
        &m.playback,
        &sent_url(Some(AUGUST)),
        &listed(&[&recent, &old]),
    );
    put_gmail(&m.playback, &sent_url(None), &listed(&[&recent, &old]));
    let all = [inbox, recent, start, old, reply];
    for message in &all {
        let id = message["id"].as_str().unwrap();
        put_gmail(&m.playback, &gmail_get_url(id), message);
    }
    put_gmail_all_mail(&m.playback, &all);
    m
}

fn ids(v: &[&str]) -> BTreeSet<String> {
    v.iter().map(|s| s.to_string()).collect()
}

async fn sync(m: &Mirror, sections: Value) -> Value {
    m.sync(json!({ "gmail": { "fetch": sections } }))
        .await
        .expect("the ingest step under playback")
}

/// The feedback that asked for this: one `since` had to do for both, so the
/// old inbox mail or the cost of all the sent mail had to go.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_section_reaches_back_as_far_as_it_says() {
    let m = account();
    let run = sync(
        &m,
        json!([
            { "labels": ["Inbox"] },
            { "labels": ["Sent"], "since": SEPTEMBER, "whole_threads": false },
        ]),
    )
    .await;
    assert_eq!(run["walked"], json!(["INBOX", "SENT"]), "{run}");
    assert_eq!(m.gmail_ids().await, ids(&[OLD_IN_INBOX, RECENT_SENT]));
}

/// Widening one section walks that label again and no other, and brings in
/// only what it newly reaches.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn widening_one_section_walks_only_its_label() {
    let m = account();
    let narrow = json!([
        { "labels": ["Inbox"] },
        { "labels": ["Sent"], "since": SEPTEMBER, "whole_threads": false },
    ]);
    sync(&m, narrow.clone()).await;

    let again = sync(&m, narrow).await;
    assert!(again.get("walked").is_none(), "{again}");
    assert_eq!(again["emails_upserted"], 0, "{again}");

    let wider = sync(
        &m,
        json!([
            { "labels": ["Inbox"] },
            { "labels": ["Sent"], "since": AUGUST, "whole_threads": false },
        ]),
    )
    .await;
    assert_eq!(wider["walked"], json!(["SENT"]), "{wider}");
    assert_eq!(wider["emails_upserted"], 1, "{wider}");
    assert_eq!(
        m.gmail_ids().await,
        ids(&[OLD_IN_INBOX, RECENT_SENT, OLD_SENT])
    );
}

/// What gets fetched is the union: a label two sections name goes as far back
/// as either asks, so adding a narrower section never loses mail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_label_in_two_sections_gets_the_wider_window() {
    let m = account();
    sync(
        &m,
        json!([
            { "labels": ["Sent"], "since": SEPTEMBER, "whole_threads": false },
            { "labels": ["Sent"], "since": AUGUST, "whole_threads": false },
        ]),
    )
    .await;
    assert_eq!(m.gmail_ids().await, ids(&[RECENT_SENT, OLD_SENT]));
}

/// A section with no `since` has the widest window of all, whichever order
/// the two come in.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_since_is_wider_than_any() {
    let m = account();
    sync(
        &m,
        json!([
            { "labels": ["Sent"], "whole_threads": false },
            { "labels": ["Sent"], "since": SEPTEMBER, "whole_threads": false },
        ]),
    )
    .await;
    assert_eq!(m.gmail_ids().await, ids(&[RECENT_SENT, OLD_SENT]));
}

/// `whole_threads` belongs to its section: the inbox's threads come whole, and
/// a sent message that is only there to name a contact does not bring its
/// conversation with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn whole_threads_follows_only_its_own_sections_labels() {
    let m = account();
    sync(
        &m,
        json!([
            { "labels": ["Sent"], "since": SEPTEMBER },
            { "labels": ["Inbox"], "whole_threads": false },
        ]),
    )
    .await;
    assert_eq!(
        m.gmail_ids().await,
        ids(&[OLD_IN_INBOX, RECENT_SENT, START_OF_RECENT_SENT]),
        "Sent's thread came whole"
    );

    let m = account();
    sync(
        &m,
        json!([
            { "labels": ["Sent"], "since": SEPTEMBER, "whole_threads": false },
            { "labels": ["Inbox"] },
        ]),
    )
    .await;
    assert_eq!(
        m.gmail_ids().await,
        ids(&[OLD_IN_INBOX, RECENT_SENT]),
        "Sent's thread stayed out"
    );
}

/// A label that does not exist costs its section and is reported under the
/// key it was written with; it does not turn the section into the whole
/// mailbox, which a section naming no labels is.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_section_of_labels_that_do_not_exist_takes_nothing() {
    let m = account();
    sync(
        &m,
        json!([
            { "labels": ["Inbox"], "whole_threads": false },
            { "labels": ["Holodeck"] },
        ]),
    )
    .await;
    assert_eq!(m.gmail_ids().await, ids(&[OLD_IN_INBOX]));
    let problems: Vec<String> = m
        .read(|db| async move {
            sqlx::query_scalar("SELECT scope_key FROM problems")
                .fetch_all(db.pool())
                .await
                .unwrap()
        })
        .await;
    assert_eq!(problems, ["config:gmail.fetch.labels:Holodeck"]);
}

/// A section naming no labels is the whole mailbox within its window, beside
/// whatever the others reach further back for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_section_with_no_labels_takes_the_whole_mailbox() {
    let m = account();
    let recent = message(RECENT_SENT, START_OF_RECENT_SENT, &["SENT"]);
    put_gmail(
        &m.playback,
        &format!("{}&q=after%3A{}", gmail_list_url(&[]), secs(SEPTEMBER)),
        &listed(&[&recent]),
    );
    let run = sync(
        &m,
        json!([
            { "since": SEPTEMBER },
            { "labels": ["Inbox"] },
        ]),
    )
    .await;
    assert_eq!(run["walked"], json!(["*", "INBOX"]), "{run}");
    assert_eq!(m.gmail_ids().await, ids(&[OLD_IN_INBOX, RECENT_SENT]));
}

/// The older keys are one section, so a source moves to `fetch` without
/// anything fetched again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn moving_the_older_keys_into_a_section_fetches_nothing_again() {
    let m = account();
    let first = m
        .sync(json!({
            "only_extract_labels": ["Sent"],
            "whole_threads": false,
            "gmail": { "since": SEPTEMBER },
        }))
        .await
        .unwrap();
    assert_eq!(first["walked"], json!(["SENT"]), "{first}");

    let moved = sync(
        &m,
        json!([{ "labels": ["Sent"], "since": SEPTEMBER, "whole_threads": false }]),
    )
    .await;
    assert!(moved.get("walked").is_none(), "{moved}");
    assert_eq!(moved["emails_upserted"], 0, "{moved}");
}
