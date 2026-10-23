//! Gmail starting from a Google Takeout of the same account: the Takeout
//! supplies the messages, the API says which exist and how they are
//! labelled, and nothing the Takeout holds is fetched whole again.
//!
//! Driven through the HTTP playback layer: no credential, no network. A
//! request the tape has no answer for fails, so a seeded message fetched
//! whole would show as a failed fetch.

use std::path::{Path, PathBuf};

use datalib_etl_email::ingest::gmail_api::{self, FetchOptions, FetchSummary};
use datalib_etl_email::ingest::labels::gmail_mailbox_id;
use datalib_etl_email::ingest::RawDb;
use serde_json::{json, Value};

use crate::support::{
    gmail_get_url, gmail_history_url, gmail_list_url, gmail_message, inbox_label, put_gmail,
    Mirror, GMAIL,
};

const ACCOUNT: &str = "t@example.test";
/// In the Takeout, and still in the account, relabeled since.
const PICARD: &str = "18c9f2a1b2c3d901";
/// In the Takeout, and archived since: no label at all now.
const RIKER: &str = "18c9f2a1b2c3d902";
/// In the Takeout, and deleted since.
const TROI: &str = "18c9f2a1b2c3d903";
/// Arrived after the Takeout was made.
const WORF: &str = "18c9f2a1b2c3d904";

const LIST_UNITS: u64 = 5;

/// The first sync seeds three messages, fetches only the one the
/// Takeout predates, takes every label from one walk per label, and
/// drops the message Gmail no longer has.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_takeout_seeds_the_mailbox_and_the_api_only_labels_it() {
    let m = account(None);
    let takeout = write_takeout(&m.playback);

    let first = run(&m, &takeout).await;
    let seeded = first.takeout.as_ref().expect("the takeout was read");
    assert_eq!(seeded.seeded, 3, "{first:?}");
    assert_eq!(
        first.messages_failed, 0,
        "a seeded message was fetched whole"
    );
    assert_eq!(first.relabeled_from_walks, 2, "{first:?}");
    assert_eq!(first.relabeled_one_by_one, 0, "{first:?}");
    // Profile and labels, the walk of the account, one walk per label,
    // and the one message the Takeout does not have.
    assert_eq!(
        first.quota_units_spent,
        1 + 1 + 3 * LIST_UNITS + u64::from(gmail_api::api::UNITS_MESSAGES_GET_RAW),
        "{first:?}"
    );

    assert_eq!(emails(&m).await, [PICARD, RIKER, WORF]);
    assert_eq!(filed(&m, PICARD).await, [real("INBOX"), real("Label_7")]);
    assert!(filed(&m, RIKER).await.is_empty());
    assert_eq!(filed(&m, WORF).await, [real("INBOX")]);
    let held: Vec<String> = m.gmail_ids().await.into_iter().collect();
    assert_eq!(held, [PICARD, RIKER, WORF]);
    assert_eq!(
        bodies(&m).await,
        3,
        "every email keeps its .eml, the seeded ones the Takeout's"
    );
}

/// Every sync reads the Takeout again. Once Gmail has listed the
/// account, a message it no longer has stays gone, and one the API has
/// relabeled keeps the API's labels.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn reading_the_takeout_again_brings_nothing_back() {
    let m = account(None);
    let takeout = write_takeout(&m.playback);
    run(&m, &takeout).await;

    put_gmail(
        &m.playback,
        &gmail_history_url("9001"),
        &json!({ "history": [], "historyId": "9001" }),
    );
    let second = run(&m, &takeout).await;
    let seeded = second.takeout.as_ref().expect("the takeout was read");
    assert_eq!(seeded.seeded, 0, "{second:?}");
    assert_eq!(seeded.held, 2, "{second:?}");
    assert_eq!(seeded.gone_upstream, 1, "{second:?}");
    assert_eq!(second.emails_upserted, 0, "{second:?}");
    assert_eq!(emails(&m).await, [PICARD, RIKER, WORF]);
    assert_eq!(filed(&m, PICARD).await, [real("INBOX"), real("Label_7")]);
}

/// A relabel of a message whose bytes are held asks for its labels
/// alone, at a third of the price of the message, when walking every
/// label would cost more.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_relabel_of_a_held_message_asks_for_its_labels_alone() {
    let m = account(None);
    let takeout = write_takeout(&m.playback);
    run(&m, &takeout).await;

    // A large account: walking every label now costs more than one
    // message's labels.
    put_profile(&m.playback, Some(100_000));
    put_gmail(
        &m.playback,
        &gmail_history_url("9001"),
        &json!({
            "history": [{ "labelsRemoved": [{ "message": { "id": PICARD } }] }],
            "historyId": "9002",
        }),
    );
    put_gmail(
        &m.playback,
        &minimal_url(PICARD),
        &json!({ "id": PICARD, "threadId": PICARD, "labelIds": ["Label_7"] }),
    );
    let relabel = run(&m, &takeout).await;
    assert_eq!(relabel.relabeled_one_by_one, 1, "{relabel:?}");
    assert_eq!(relabel.messages_failed, 0, "{relabel:?}");
    assert_eq!(
        relabel.quota_units_spent,
        1 + 1 + 2 + u64::from(gmail_api::api::UNITS_MESSAGES_GET_MINIMAL),
        "profile, labels, history and one labels-only get: {relabel:?}"
    );
    assert_eq!(filed(&m, PICARD).await, [real("Label_7")]);
}

/// `t@example.test` now: Picard relabeled, Riker archived, Troi gone,
/// Worf new.
fn account(messages_total: Option<u64>) -> Mirror {
    let m = Mirror::new();
    put_profile(&m.playback, messages_total);
    put_gmail(
        &m.playback,
        &format!("{GMAIL}/labels"),
        &json!({ "labels": [inbox_label(), { "id": "Label_7", "name": "away team", "type": "user" }] }),
    );
    put_gmail(
        &m.playback,
        &gmail_list_url(&[]),
        &json!({ "messages": [{ "id": PICARD }, { "id": RIKER }, { "id": WORF }] }),
    );
    put_gmail(
        &m.playback,
        &gmail_list_url(&["INBOX"]),
        &json!({ "messages": [{ "id": PICARD }, { "id": WORF }] }),
    );
    put_gmail(
        &m.playback,
        &gmail_list_url(&["Label_7"]),
        &json!({ "messages": [{ "id": PICARD }] }),
    );
    put_gmail(
        &m.playback,
        &gmail_get_url(WORF),
        &gmail_message(WORF, &["INBOX"], "new since the takeout"),
    );
    m
}

fn put_profile(playback: &Path, messages_total: Option<u64>) {
    let mut profile = json!({ "emailAddress": ACCOUNT, "historyId": "9001" });
    if let Some(total) = messages_total {
        profile["messagesTotal"] = Value::from(total);
    }
    put_gmail(playback, &format!("{GMAIL}/profile"), &profile);
}

/// The account's Takeout from before: Picard, Riker and Troi, all in the
/// inbox, each its own thread.
fn write_takeout(dir: &Path) -> PathBuf {
    let path = dir
        .join("Takeout")
        .join("All mail Including Spam and Trash.mbox");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut mbox = String::new();
    for (id, subject) in [
        (PICARD, "make it so"),
        (RIKER, "away team report"),
        (TROI, "I sense deception"),
    ] {
        let decimal = u64::from_str_radix(id, 16).unwrap();
        mbox.push_str(&format!(
            "From {decimal}@xxx Mon Jan 05 09:00:00 +0000 2026\n\
             X-GM-THRID: {decimal}\n\
             X-Gmail-Labels: Inbox\n\
             Message-Id: <{id}@example.test>\n\
             From: sender@example.test\n\
             To: {ACCOUNT}\n\
             Subject: {subject}\n\
             Date: Mon, 5 Jan 2026 09:00:00 +0000\n\
             \n\
             body of {subject}\n\n"
        ));
    }
    std::fs::write(&path, mbox).unwrap();
    path.parent().unwrap().to_path_buf()
}

fn minimal_url(id: &str) -> String {
    format!("{GMAIL}/messages/{id}?format=MINIMAL")
}

fn real(label_id: &str) -> String {
    gmail_mailbox_id(ACCOUNT, label_id)
}

async fn run(m: &Mirror, takeout: &Path) -> FetchSummary {
    let takeout = takeout.to_path_buf();
    m.run(|db| {
        gmail_api::fetch(FetchOptions {
            takeout: Some(takeout),
            ..FetchOptions::new(db)
        })
    })
    .await
    .expect("gmail fetch under playback")
}

async fn emails(m: &Mirror) -> Vec<String> {
    m.read(|db: RawDb| async move {
        sqlx::query_scalar("SELECT id FROM emails ORDER BY id")
            .fetch_all(db.pool())
            .await
            .unwrap()
    })
    .await
}

/// The mailboxes the message with this Gmail id is filed under, by its
/// join rows. The ids here are sixteen hex digits, so the email row's id
/// is the same.
async fn filed(m: &Mirror, gmail_id: &'static str) -> Vec<String> {
    m.read(|db: RawDb| async move {
        sqlx::query_scalar(
            "SELECT mailbox_id FROM email_mailboxes WHERE email_id = ? ORDER BY mailbox_id",
        )
        .bind(gmail_id)
        .fetch_all(db.pool())
        .await
        .unwrap()
    })
    .await
}

async fn bodies(m: &Mirror) -> i64 {
    m.read(|db: RawDb| async move {
        sqlx::query_scalar(
            "SELECT count(*) FROM emails e JOIN email_blobs b
             ON b.email_id = e.id AND b.blob_id = e.blob_id WHERE b.blake3 IS NOT NULL",
        )
        .fetch_one(db.pool())
        .await
        .unwrap()
    })
    .await
}
