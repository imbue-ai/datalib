//! A Gmail label's row is keyed by Google's label id: a rename renames
//! the row, a deleted label takes itself off every email, and a row an
//! mbox import keyed by name moves onto the real id once the API has
//! listed it. Driven through the HTTP playback layer: no credential, no
//! network.

use std::collections::BTreeMap;
use std::path::Path;

use datalib_etl_email::ingest::gmail_api::{self, FetchOptions, FetchSummary};
use datalib_etl_email::ingest::labels::{gmail_mailbox_id, mailbox_id};
use datalib_etl_email::ingest::{mbox, RawDb};
use serde_json::{json, Value};

use crate::support::{
    gmail_get_url, gmail_history_url, gmail_list_url, gmail_message, inbox_label, put_gmail,
    put_gmail_account, Mirror, GMAIL,
};

const ACCOUNT: &str = "t@example.test";
const UNDER_LIB: &str = "18c9f2a1b2c3d701";
const UNDER_TRAVEL: &str = "18c9f2a1b2c3d702";
/// The Takeout message's Gmail id, as its `From ` line spells it, and as
/// the row's id spells it.
const TAKEOUT_DECIMAL: &str = "1853466712473707184";
const TAKEOUT_EMAIL: &str = "19b8d627a801a2b0";

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gmail_label_row_follows_the_label() {
    // One test, scenarios in sequence: `PLAYBACK_ENV` is process-global.
    a_renamed_label_keeps_its_row_and_a_deleted_one_comes_off_its_mail().await;
    a_takeout_import_moves_onto_the_real_label_ids().await;
    a_takeout_import_after_the_api_files_under_the_real_ids().await;
    a_takeout_label_no_message_carries_goes().await;
    a_label_listing_that_names_nothing_takes_no_label_off_any_mail().await;
}

/// `labels.list` answering with no `labels` key read as an account with
/// no labels, and every Gmail mailbox the store held was emptied: every
/// email lost every label. Every account has the system labels, so a
/// reply without the list is malformed and fails the run, and a list
/// that names nothing plans no deletions.
async fn a_label_listing_that_names_nothing_takes_no_label_off_any_mail() {
    let m = Mirror::new();
    put_labels(
        &m.playback,
        &[("Label_7", "datalib"), ("Label_9", "travel")],
    );
    put_gmail(
        &m.playback,
        &gmail_list_url(&[]),
        &json!({ "messages": [{ "id": UNDER_LIB }, { "id": UNDER_TRAVEL }] }),
    );
    for (id, labels) in [
        (UNDER_LIB, &["INBOX", "Label_7"][..]),
        (UNDER_TRAVEL, &["INBOX", "Label_9"][..]),
    ] {
        put_gmail(
            &m.playback,
            &gmail_get_url(id),
            &gmail_message(id, labels, id),
        );
    }
    run_gmail(&m, &[]).await;
    let before = State::read(&m).await;
    assert_eq!(before.mailboxes.len(), 3);
    put_gmail(
        &m.playback,
        &gmail_history_url("1000"),
        &json!({ "historyId": "1000" }),
    );

    put_gmail(&m.playback, &format!("{GMAIL}/labels"), &json!({}));
    let malformed = m.run(|db| gmail_api::fetch(FetchOptions::new(db))).await;
    assert!(
        malformed.is_err(),
        "a reply without its list is not a listing"
    );
    let after = State::read(&m).await;
    assert_eq!(after.mailboxes, before.mailboxes);
    assert_eq!(after.filed(UNDER_LIB), before.filed(UNDER_LIB));

    put_gmail(
        &m.playback,
        &format!("{GMAIL}/labels"),
        &json!({ "labels": [] }),
    );
    let empty = run_gmail(&m, &[]).await;
    assert_eq!(empty.mailboxes_destroyed, 0, "{empty:?}");
    let after = State::read(&m).await;
    assert_eq!(after.mailboxes, before.mailboxes);
    assert_eq!(after.filed(UNDER_LIB), before.filed(UNDER_LIB));
    assert_eq!(after.filed(UNDER_TRAVEL), before.filed(UNDER_TRAVEL));
}

async fn a_takeout_label_no_message_carries_goes() {
    let m = Mirror::new();
    let dir = tempfile::tempdir().unwrap();
    import_takeout(&m, dir.path(), "Inbox,Old").await;
    import_takeout(&m, dir.path(), "Inbox,Renamed Since").await;

    let state = State::read(&m).await;
    assert_eq!(
        state.filed(TAKEOUT_EMAIL),
        sorted(vec![named("Inbox"), named("Renamed Since")])
    );
    assert!(!state.mailboxes.contains_key(&named("Old")));
}

async fn a_renamed_label_keeps_its_row_and_a_deleted_one_comes_off_its_mail() {
    let m = Mirror::new();
    put_labels(
        &m.playback,
        &[("Label_7", "datalib"), ("Label_9", "travel")],
    );
    put_gmail(
        &m.playback,
        &gmail_list_url(&[]),
        &json!({ "messages": [{ "id": UNDER_LIB }, { "id": UNDER_TRAVEL }] }),
    );
    for (id, labels) in [
        (UNDER_LIB, &["INBOX", "Label_7"][..]),
        (UNDER_TRAVEL, &["INBOX", "Label_9"][..]),
    ] {
        put_gmail(
            &m.playback,
            &gmail_get_url(id),
            &gmail_message(id, labels, id),
        );
    }
    let first = run_gmail(&m, &[]).await;
    assert_eq!(first.emails_upserted, 2, "{first:?}");

    // `datalib` is renamed and `travel` deleted; history reports nothing
    // about either message.
    put_labels(&m.playback, &[("Label_7", "datalib-renamed")]);
    put_gmail(
        &m.playback,
        &gmail_history_url("1000"),
        &json!({ "historyId": "1000" }),
    );
    let second = run_gmail(&m, &[]).await;
    assert_eq!(second.emails_upserted, 0, "{second:?}");
    assert_eq!(second.mailboxes_destroyed, 1, "{second:?}");

    let state = State::read(&m).await;
    assert_eq!(
        state.mailboxes,
        BTreeMap::from([
            (real("INBOX"), "Inbox".to_string()),
            (real("Label_7"), "datalib-renamed".to_string()),
        ])
    );
    assert_eq!(state.filed(UNDER_LIB), vec![real("INBOX"), real("Label_7")]);
    assert_eq!(state.filed(UNDER_TRAVEL), vec![real("INBOX")]);
}

async fn a_takeout_import_moves_onto_the_real_label_ids() {
    let m = Mirror::new();
    let dir = tempfile::tempdir().unwrap();
    import_takeout(&m, dir.path(), "Inbox,datalib,Long Gone").await;
    let before = State::read(&m).await;
    assert_eq!(
        before.filed(TAKEOUT_EMAIL),
        sorted(vec![named("Inbox"), named("datalib"), named("Long Gone")])
    );

    // A filtered walk that lists nothing: the Takeout message is never
    // refetched, so only the re-key can move it.
    put_labels(
        &m.playback,
        &[("Label_7", "datalib"), ("Label_9", "travel")],
    );
    put_gmail(&m.playback, &gmail_list_url(&["Label_9"]), &json!({}));
    let run = run_gmail(&m, &["travel"]).await;
    assert_eq!(run.emails_refiled, 2, "{run:?}");

    let after = State::read(&m).await;
    assert_eq!(
        after.filed(TAKEOUT_EMAIL),
        sorted(vec![real("INBOX"), real("Label_7"), named("Long Gone")]),
        "a label Gmail no longer lists keeps its name-keyed row"
    );
    assert_eq!(
        after.payload_filed(TAKEOUT_EMAIL),
        after.filed(TAKEOUT_EMAIL)
    );
    assert!(!after.mailboxes.contains_key(&named("Inbox")));
    assert!(!after.mailboxes.contains_key(&named("datalib")));
    assert_eq!(after.mailboxes[&named("Long Gone")], "Long Gone");
}

async fn a_takeout_import_after_the_api_files_under_the_real_ids() {
    let m = Mirror::new();
    put_labels(
        &m.playback,
        &[("Label_7", "datalib"), ("Label_9", "travel")],
    );
    put_gmail(&m.playback, &gmail_list_url(&["Label_9"]), &json!({}));
    run_gmail(&m, &["travel"]).await;

    let dir = tempfile::tempdir().unwrap();
    import_takeout(&m, dir.path(), "Inbox,datalib,Takeout Only").await;

    let state = State::read(&m).await;
    assert_eq!(
        state.filed(TAKEOUT_EMAIL),
        sorted(vec![real("INBOX"), real("Label_7"), named("Takeout Only")])
    );
    assert!(!state.mailboxes.contains_key(&named("datalib")));
    assert_eq!(state.mailboxes[&real("Label_7")], "datalib");
}

fn real(label_id: &str) -> String {
    gmail_mailbox_id(ACCOUNT, label_id)
}

fn named(label: &str) -> String {
    mailbox_id(ACCOUNT, label)
}

fn sorted(mut v: Vec<String>) -> Vec<String> {
    v.sort();
    v
}

fn put_labels(playback: &Path, user: &[(&str, &str)]) {
    let mut labels = vec![inbox_label()];
    labels.extend(
        user.iter()
            .map(|(id, name)| json!({ "id": id, "name": name, "type": "user" })),
    );
    put_gmail_account(playback, "1000", Value::Array(labels));
}

async fn run_gmail(m: &Mirror, labels: &[&str]) -> FetchSummary {
    m.run(|db| {
        let mut opts = FetchOptions::new(db);
        opts.only_labels = labels.iter().map(|s| s.to_string()).collect();
        gmail_api::fetch(opts)
    })
    .await
    .expect("gmail fetch under playback")
}

async fn import_takeout(m: &Mirror, dir: &Path, labels: &str) {
    let path = dir.join("All mail.mbox");
    std::fs::write(
        &path,
        format!(
            "From {TAKEOUT_DECIMAL}@xxx Mon Jan 05 09:00:00 +0000 2026\n\
             X-GM-THRID: {TAKEOUT_DECIMAL}\n\
             X-Gmail-Labels: {labels}\n\
             Message-Id: <takeout-1@example.test>\n\
             From: sender@example.test\n\
             To: {ACCOUNT}\n\
             Subject: from the export\n\
             Date: Mon, 5 Jan 2026 09:00:00 +0000\n\
             \n\
             body\n\n"
        ),
    )
    .unwrap();
    m.read(|db: RawDb| async move {
        mbox::fetch(mbox::FetchOptions {
            input_path: path,
            account_config: mbox::MboxAccountConfig {
                account_id: Some(ACCOUNT.to_string()),
                ..Default::default()
            },
            ..mbox::FetchOptions::new(db)
        })
        .await
        .expect("mbox fetch")
    })
    .await;
}

/// The mailbox rows, and where each email is filed — by its join rows
/// and by its payload.
struct State {
    mailboxes: BTreeMap<String, String>,
    joins: BTreeMap<String, Vec<String>>,
    payloads: BTreeMap<String, Vec<String>>,
}

impl State {
    async fn read(m: &Mirror) -> Self {
        m.read(|db: RawDb| async move {
            let mailboxes = db.mailbox_names(ACCOUNT).await.unwrap();
            let mut joins: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for (e, ids) in db.load_email_joins().await.unwrap().mailboxes {
                joins.insert(e, sorted(ids));
            }
            let rows: Vec<(String, String)> =
                sqlx::query_as("SELECT id, json(payload) FROM emails")
                    .fetch_all(db.pool())
                    .await
                    .unwrap();
            let payloads = rows
                .into_iter()
                .map(|(id, p)| {
                    let v: Value = serde_json::from_str(&p).unwrap();
                    let ids = v["mailboxIds"]
                        .as_object()
                        .map(|o| o.keys().cloned().collect())
                        .unwrap_or_default();
                    (id, sorted(ids))
                })
                .collect();
            State {
                mailboxes,
                joins,
                payloads,
            }
        })
        .await
    }

    fn filed(&self, email: &str) -> Vec<String> {
        self.joins.get(email).cloned().unwrap_or_default()
    }

    fn payload_filed(&self, email: &str) -> Vec<String> {
        self.payloads.get(email).cloned().unwrap_or_default()
    }
}
