//! Gmail: each email carries a line of its body as plain text, and a read
//! mark saying whether its thread is muted, so a list of mail needs neither
//! the `.eml` nor another request.
//!
//! Driven through the HTTP playback layer: no credential, no network.

use std::collections::BTreeMap;

use datalib_etl_email::ingest::gmail_api::{self, FetchOptions, FetchSummary};
use datalib_etl_email::ingest::labels::gmail_mailbox_id;
use datalib_etl_email::ingest::RawDb;
use serde_json::{json, Value};

use crate::support::{
    gmail_get_url, gmail_history_url, gmail_list_url, gmail_message, gmail_muted_url, inbox_label,
    put_gmail, put_gmail_account, put_gmail_muted, put_gmail_response, Mirror, GMAIL,
};

const PICARD: &str = "18c9f2a1b2c3d801";
const RIKER: &str = "18c9f2a1b2c3d802";
const ACCOUNT: &str = "t@example.test";

fn labels() -> Value {
    json!([inbox_label(), { "id": "Label_7", "name": "away team", "type": "user" }])
}

fn mirror() -> Mirror {
    let m = Mirror::new();
    put_gmail_account(&m.playback, "9001", labels());
    put_gmail(
        &m.playback,
        &gmail_list_url(&[]),
        &json!({ "messages": [{ "id": PICARD }, { "id": RIKER }] }),
    );
    for id in [PICARD, RIKER] {
        put_gmail(
            &m.playback,
            &gmail_get_url(id),
            &gmail_message(id, &["INBOX"], id),
        );
    }
    m
}

async fn run(m: &Mirror) -> FetchSummary {
    m.run(|db| gmail_api::fetch(FetchOptions::new(db)))
        .await
        .expect("gmail fetch under playback")
}

async fn previews(m: &Mirror) -> BTreeMap<String, Option<String>> {
    m.read(|db: RawDb| async move {
        let rows: Vec<(String, Option<String>)> =
            sqlx::query_as("SELECT id, preview FROM emails ORDER BY id")
                .fetch_all(db.pool())
                .await
                .unwrap();
        rows.into_iter().collect()
    })
    .await
}

async fn mutes(m: &Mirror) -> BTreeMap<String, Option<i64>> {
    m.read(|db: RawDb| async move {
        let rows: Vec<(String, Option<i64>)> =
            sqlx::query_as("SELECT id, muted FROM read_marks ORDER BY id")
                .fetch_all(db.pool())
                .await
                .unwrap();
        rows.into_iter().collect()
    })
    .await
}

fn marks(picard: Option<i64>, riker: Option<i64>) -> BTreeMap<String, Option<i64>> {
    [(PICARD.to_string(), picard), (RIKER.to_string(), riker)].into()
}

/// The snippet lands in the column as text, and stays out of the envelope,
/// which holds no part of the body.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn gmails_snippet_is_the_preview_as_plain_text() {
    let m = mirror();
    let mut message = gmail_message(PICARD, &["INBOX"], PICARD);
    message["snippet"] = json!("Captain&#39;s log: tea &amp; biscuits");
    put_gmail(&m.playback, &gmail_get_url(PICARD), &message);
    run(&m).await;

    assert_eq!(
        previews(&m).await[PICARD].as_deref(),
        Some("Captain's log: tea & biscuits")
    );
    let in_payload: i64 = m
        .read(|db: RawDb| async move {
            sqlx::query_scalar("SELECT count(*) FROM emails WHERE payload -> '$.preview' NOT NULL")
                .fetch_one(db.pool())
                .await
                .unwrap()
        })
        .await;
    assert_eq!(in_payload, 0);
}

/// A label walk relabels a held message for almost nothing, and names no
/// snippet: the row's own has to survive it, or a Gmail snippet would turn
/// into a different line each time a label moved.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relabel_from_the_walks_keeps_the_preview() {
    let m = Mirror::new();
    put_gmail_account(&m.playback, "9001", labels());
    let ids: Vec<Value> = [PICARD]
        .iter()
        .map(|id| json!({ "id": id, "threadId": id }))
        .collect();
    put_gmail(
        &m.playback,
        &gmail_list_url(&[]),
        &json!({ "messages": ids }),
    );
    put_gmail(
        &m.playback,
        &gmail_get_url(PICARD),
        &gmail_message(PICARD, &["INBOX"], "tea"),
    );
    run(&m).await;

    // A profile count this small is what makes the walk the cheaper way.
    put_gmail(
        &m.playback,
        &format!("{GMAIL}/profile"),
        &json!({ "emailAddress": ACCOUNT, "historyId": "9002", "messagesTotal": 1 }),
    );
    put_gmail(
        &m.playback,
        &gmail_history_url("9001"),
        &json!({
            "history": [{ "labelsAdded": [{ "message": { "id": PICARD, "threadId": PICARD } }] }],
            "historyId": "9002",
        }),
    );
    put_gmail(
        &m.playback,
        &gmail_list_url(&["INBOX"]),
        &json!({ "messages": [{ "id": PICARD, "threadId": PICARD }] }),
    );
    put_gmail(
        &m.playback,
        &gmail_list_url(&["Label_7"]),
        &json!({ "messages": [{ "id": PICARD, "threadId": PICARD }] }),
    );
    let second = run(&m).await;
    assert_eq!(second.relabeled_from_walks, 1, "{second:?}");

    assert_eq!(
        previews(&m).await[PICARD].as_deref(),
        Some("snippet of tea")
    );
    let labelled: Vec<String> = m
        .read(|db: RawDb| async move {
            sqlx::query_scalar("SELECT mailbox_id FROM email_mailboxes ORDER BY mailbox_id")
                .fetch_all(db.pool())
                .await
                .unwrap()
        })
        .await;
    assert_eq!(
        labelled,
        [
            gmail_mailbox_id(ACCOUNT, "INBOX"),
            gmail_mailbox_id(ACCOUNT, "Label_7")
        ]
    );
}

/// A store from before the column has mail with no preview. It is filled
/// from the bytes held, at no quota: `messages.get` would cost 20 units a
/// message for what the store can work out itself.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_held_message_with_no_preview_gets_one_from_its_bytes() {
    let m = mirror();
    run(&m).await;
    m.read(|db: RawDb| async move {
        sqlx::query("UPDATE emails SET preview = NULL WHERE id = ?")
            .bind(PICARD)
            .execute(db.pool())
            .await
            .unwrap();
    })
    .await;

    put_gmail(
        &m.playback,
        &gmail_history_url("9001"),
        &json!({ "historyId": "9001" }),
    );
    let second = run(&m).await;
    assert_eq!(second.emails_upserted, 0, "{second:?}");

    let got = previews(&m).await;
    assert_eq!(
        got[PICARD].as_deref(),
        Some(format!("body of {PICARD}").as_str())
    );
    assert_eq!(
        got[RIKER].as_deref(),
        Some(format!("snippet of {RIKER}").as_str())
    );
}

/// Muting is no label, so nothing says so but the search. Each run brings
/// every mark into line with it, on a message no fetch touched as well.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_search_decides_who_is_muted_and_unmuting_lands() {
    let m = mirror();
    put_gmail_muted(&m.playback, &[RIKER]);
    let first = run(&m).await;
    assert_eq!(mutes(&m).await, marks(Some(0), Some(1)));
    assert_eq!(first.mutes_changed, 0, "the fetch wrote them: {first:?}");

    put_gmail(
        &m.playback,
        &gmail_history_url("9001"),
        &json!({ "historyId": "9001" }),
    );
    put_gmail_muted(&m.playback, &[PICARD]);
    let second = run(&m).await;
    assert_eq!(second.emails_upserted, 0, "{second:?}");
    assert_eq!(second.mutes_changed, 2, "{second:?}");
    assert_eq!(mutes(&m).await, marks(Some(1), Some(0)));

    let third = run(&m).await;
    assert_eq!(third.mutes_changed, 0, "nothing moved: {third:?}");
}

/// A search that fails says nothing about who is muted, so no mark moves and
/// a `problems` row says why; the next run that gets an answer clears it. A
/// failed walk read as "nobody" would unmute every thread.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_search_moves_no_mark() {
    let m = mirror();
    put_gmail_muted(&m.playback, &[RIKER]);
    run(&m).await;

    put_gmail(
        &m.playback,
        &gmail_history_url("9001"),
        &json!({ "historyId": "9001" }),
    );
    put_gmail_response(
        &m.playback,
        &gmail_muted_url(),
        &crate::jmap_tape::status(400),
    );
    let second = run(&m).await;
    assert_eq!(second.mutes_changed, 0, "{second:?}");
    assert_eq!(mutes(&m).await, marks(Some(0), Some(1)));
    assert_eq!(problem_rows(&m).await, ["listing:messages.list muted"]);

    put_gmail_muted(&m.playback, &[RIKER]);
    run(&m).await;
    assert!(problem_rows(&m).await.is_empty());
}

async fn problem_rows(m: &Mirror) -> Vec<String> {
    m.read(|db: RawDb| async move {
        sqlx::query_scalar("SELECT scope_key FROM problems ORDER BY scope_key")
            .fetch_all(db.pool())
            .await
            .unwrap()
    })
    .await
}

/// A message fetched while the search has failed is not known either way,
/// and is left for the next answer rather than called unmuted.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_fetched_without_an_answer_is_not_called_unmuted() {
    let m = mirror();
    put_gmail_response(
        &m.playback,
        &gmail_muted_url(),
        &crate::jmap_tape::status(400),
    );
    run(&m).await;
    assert_eq!(mutes(&m).await, marks(None, None));

    put_gmail(
        &m.playback,
        &gmail_history_url("9001"),
        &json!({ "historyId": "9001" }),
    );
    put_gmail_muted(&m.playback, &[RIKER]);
    run(&m).await;
    assert_eq!(mutes(&m).await, marks(Some(0), Some(1)));
}
