//! Gmail: what `history.list` names is listed in one transaction with
//! the new `historyId`, and fetched because it is listed anew. So a
//! label change on a message already mirrored lands, and a deletion
//! takes the email, its mapping and its place in its thread together.
//!
//! Driven through the HTTP playback layer: no credential, no network.

use datalib_etl_email::ingest::gmail_api::{self, FetchOptions, FetchSummary};
use datalib_etl_email::ingest::labels::gmail_mailbox_id;
use datalib_etl_email::ingest::RawDb;
use serde_json::{json, Value};

use crate::support::{
    gmail_get_url, gmail_history_url, gmail_list_url, gmail_message, inbox_label, put_gmail,
    put_gmail_account, Mirror,
};

const PICARD: &str = "18c9f2a1b2c3d801";
const RIKER: &str = "18c9f2a1b2c3d802";
const ACCOUNT: &str = "t@example.test";

fn labels() -> Value {
    json!([inbox_label(), { "id": "Label_7", "name": "away team", "type": "user" }])
}

/// Two messages in one thread, both in the inbox.
fn mirror() -> Mirror {
    let m = Mirror::new();
    put_gmail_account(&m.playback, "9001", labels());
    put_gmail(
        &m.playback,
        &gmail_list_url(&[]),
        &json!({ "messages": [{ "id": PICARD }, { "id": RIKER }] }),
    );
    for id in [PICARD, RIKER] {
        let mut message = gmail_message(id, &["INBOX"], id);
        message["threadId"] = json!(PICARD);
        put_gmail(&m.playback, &gmail_get_url(id), &message);
    }
    m
}

/// The fetch once skipped any message it already held, and a relabel is
/// only ever about a message already held: no label change after the
/// first sync reached the mirror.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_label_change_on_a_mirrored_message_lands() {
    let m = mirror();
    run(&m).await;
    assert_eq!(
        filed(&m, PICARD).await,
        [gmail_mailbox_id(ACCOUNT, "INBOX")]
    );

    put_gmail(
        &m.playback,
        &gmail_history_url("9001"),
        &json!({
            "history": [{ "labelsAdded": [{ "message": { "id": PICARD } }] }],
            "historyId": "9002",
        }),
    );
    let mut relabeled = gmail_message(PICARD, &["INBOX", "Label_7"], PICARD);
    relabeled["threadId"] = json!(PICARD);
    put_gmail(&m.playback, &gmail_get_url(PICARD), &relabeled);
    let second = run(&m).await;
    assert_eq!(second.emails_upserted, 1, "{second:?}");
    assert_eq!(
        filed(&m, PICARD).await,
        [
            gmail_mailbox_id(ACCOUNT, "INBOX"),
            gmail_mailbox_id(ACCOUNT, "Label_7")
        ]
    );
}

/// A deletion `history.list` reports removes the email, what is held for
/// it and its place in its thread in one transaction.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_deleted_message_goes_with_its_mapping_and_leaves_its_thread() {
    let m = mirror();
    run(&m).await;
    assert_eq!(thread_sizes(&m).await, [2]);

    put_gmail(
        &m.playback,
        &gmail_history_url("9001"),
        &json!({
            "history": [{ "messagesDeleted": [{ "message": { "id": RIKER } }] }],
            "historyId": "9002",
        }),
    );
    let second = run(&m).await;
    assert_eq!(second.emails_destroyed, 1, "{second:?}");
    assert_eq!(
        m.gmail_ids().await.into_iter().collect::<Vec<_>>(),
        [PICARD]
    );
    assert_eq!(thread_sizes(&m).await, [1]);
    let emails: i64 = m
        .read(|db: RawDb| async move {
            sqlx::query_scalar("SELECT count(*) FROM emails")
                .fetch_one(db.pool())
                .await
                .unwrap()
        })
        .await;
    assert_eq!(emails, 1);
}

async fn run(m: &Mirror) -> FetchSummary {
    m.run(|db| gmail_api::fetch(FetchOptions::new(db)))
        .await
        .expect("gmail fetch under playback")
}

/// The mailboxes the message with this Gmail id is filed under.
async fn filed(m: &Mirror, gmail_id: &'static str) -> Vec<String> {
    m.read(|db: RawDb| async move {
        sqlx::query_scalar(
            "SELECT j.mailbox_id FROM fetched_messages f
             JOIN email_mailboxes j ON j.email_id = f.email_id
             WHERE f.id = ? ORDER BY j.mailbox_id",
        )
        .bind(gmail_id)
        .fetch_all(db.pool())
        .await
        .unwrap()
    })
    .await
}

async fn thread_sizes(m: &Mirror) -> Vec<i64> {
    m.read(|db: RawDb| async move {
        sqlx::query_scalar("SELECT email_count FROM threads ORDER BY id")
            .fetch_all(db.pool())
            .await
            .unwrap()
    })
    .await
}
