//! Fastmail (JMAP): what `Email/changes` or an `Email/query` walk names
//! is listed, and what is listed and not held is fetched. So a fetch
//! that fails leaves its emails to the next run without anything being
//! listed again, a mailbox the label filter newly admits is enumerated
//! until an enumeration of it finishes, and a thread row is written
//! with its emails.

use datalib_etl::doltlite_raw as dr;
use datalib_etl::read_marks::{self, ReadMark};
use datalib_etl_email::ingest::{FetchOptions, FetchSummary, RawDb};

use crate::jmap_tape::{changes_args, email_get_args, query_args, Account, Tape, HOST};
use crate::support::Mirror;

/// Any failure of the delta once sent the run down a full `Email/query`
/// enumeration and an `Email/get` of the whole account. Only a server
/// that cannot calculate the changes calls for that. Here every
/// `Email/query` is refused, so an enumeration would leave its row.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_failed_fetch_leaves_its_emails_owed_and_enumerates_nothing() {
    let m = Mirror::new();
    let tape = Tape::new(&m.playback);
    let mut a = Account::new(&[("MB1", "Inbox")], &[("M1", &["MB1"]), ("M2", &["MB1"])]);
    tape.serve(&a);
    run(&m, &[]).await.expect("first run");

    // `M1` is flagged upstream, and the `Email/get` for it fails.
    a.email_state = "email-2".into();
    a.emails.get_mut("M1").unwrap().flagged = true;
    tape.serve(&a);
    tape.email_changes("email-1", &[], &["M1"], &[], "email-2", false);
    tape.refuse("Email/get", email_get_args(&["M1"]), 400);
    tape.refuse("Email/query", query_args(0, &[]), 400);
    run(&m, &[])
        .await
        .expect("a fetch that fails does not fail the run");
    assert_eq!(problems(&m).await, ["listed_messages:M1"]);
    assert_eq!(
        email_state(&m).await,
        "email-2",
        "the state waited on the fetch"
    );
    assert!(keywords(&m, "M1").await.is_empty());

    // Nothing names `M1` again: it is fetched because it is owed.
    tape.gets(&a, &["M1"]);
    let third = run(&m, &[]).await.expect("third run");
    assert_eq!(third.emails_upserted, 1, "{third:?}");
    assert_eq!(keywords(&m, "M1").await, ["$flagged"]);
    assert!(problems(&m).await.is_empty(), "{:?}", problems(&m).await);
}

/// Whether an email was read is its read mark and nothing else: `$seen`
/// is in no keyword row and no payload, and an email marked unread
/// upstream moves its mark and the store's version while its row stays
/// as it was.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn marking_an_email_unread_upstream_moves_only_its_read_mark() {
    let m = Mirror::new();
    let tape = Tape::new(&m.playback);
    let mut a = Account::new(&[("MB1", "Inbox")], &[("M1", &["MB1"]), ("M2", &["MB1"])]);
    tape.serve(&a);
    run(&m, &[]).await.expect("first run");
    assert_eq!(marks(&m).await, [("M1", true), ("M2", true)]);
    assert!(strings(&m, "SELECT keyword FROM email_keywords")
        .await
        .is_empty());
    let rows = email_rows(&m).await;
    assert!(rows.iter().all(|r| !r.contains("$seen")), "{rows:?}");
    let version = content_commit(&m).await;

    a.email_state = "email-2".into();
    a.emails.get_mut("M1").unwrap().seen = false;
    tape.serve(&a);
    tape.email_changes("email-1", &[], &["M1"], &[], "email-2", false);
    tape.gets(&a, &["M1"]);
    run(&m, &[]).await.expect("second run");

    assert_eq!(marks(&m).await, [("M1", false), ("M2", true)]);
    assert_eq!(email_rows(&m).await, rows);
    assert_ne!(content_commit(&m).await, version);
}

/// `$muted` is a mark, as `$seen` is: muting moves the mark and leaves the
/// row and its keywords as they were.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn muting_upstream_moves_only_the_read_mark() {
    let m = Mirror::new();
    let tape = Tape::new(&m.playback);
    let mut a = Account::new(&[("MB1", "Inbox")], &[("M1", &["MB1"]), ("M2", &["MB1"])]);
    tape.serve(&a);
    run(&m, &[]).await.expect("first run");
    let rows = email_rows(&m).await;
    assert_eq!(muted(&m).await, [0, 0]);

    a.email_state = "email-2".into();
    a.emails.get_mut("M1").unwrap().muted = true;
    tape.serve(&a);
    tape.email_changes("email-1", &[], &["M1"], &[], "email-2", false);
    tape.gets(&a, &["M1"]);
    run(&m, &[]).await.expect("second run");

    assert_eq!(muted(&m).await, [1, 0]);
    assert_eq!(email_rows(&m).await, rows);
    assert!(strings(&m, "SELECT keyword FROM email_keywords")
        .await
        .is_empty());
}

/// The server's preview is the row's, empty or not, and stays out of the
/// envelope. A row from before the column gets one from its `.eml`, with no
/// `Email/get` for it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_servers_preview_is_kept_and_an_old_row_gets_one_from_its_bytes() {
    let m = Mirror::new();
    let tape = Tape::new(&m.playback);
    let mut a = Account::new(&[("MB1", "Inbox")], &[("M1", &["MB1"]), ("M2", &["MB1"])]);
    a.emails.get_mut("M2").unwrap().preview = String::new();
    tape.serve(&a);
    run(&m, &[]).await.expect("first run");
    assert_eq!(
        strings(&m, "SELECT preview FROM emails ORDER BY id").await,
        ["Preview of log M1", ""]
    );
    assert!(strings(
        &m,
        "SELECT id FROM emails WHERE payload -> '$.preview' NOT NULL"
    )
    .await
    .is_empty());

    m.read(|db: RawDb| async move {
        sqlx::query("UPDATE emails SET preview = NULL WHERE id = 'M1'")
            .execute(db.pool())
            .await
            .unwrap();
    })
    .await;
    tape.refuse("Email/get", email_get_args(&["M1"]), 400);
    let second = run(&m, &[]).await.expect("second run");
    assert_eq!(second.emails_upserted, 0, "{second:?}");
    assert_eq!(
        strings(&m, "SELECT preview FROM emails ORDER BY id").await,
        ["body of M1", ""]
    );
}

/// An `Email/changes` that fails is a row, and what is stored stands;
/// one the server cannot calculate lists and fetches the account again.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn only_a_state_the_server_cannot_replay_enumerates_the_account() {
    let m = Mirror::new();
    let tape = Tape::new(&m.playback);
    let mut a = Account::new(&[("MB1", "Inbox")], &[("M1", &["MB1"]), ("M2", &["MB1"])]);
    tape.serve(&a);
    run(&m, &[]).await.expect("first run");

    tape.refuse("Email/changes", changes_args("email-1"), 400);
    tape.refuse("Email/query", query_args(0, &[]), 400);
    run(&m, &[])
        .await
        .expect("a delta that fails does not fail the run");
    assert_eq!(problems(&m).await, ["listing:Email/changes"]);

    // The state aged out, `M2` was destroyed meanwhile and `M1` flagged.
    a.email_state = "email-9".into();
    a.emails.remove("M2");
    a.emails.get_mut("M1").unwrap().flagged = true;
    tape.serve(&a);
    tape.method_error(
        "Email/changes",
        changes_args("email-1"),
        "cannotCalculateChanges",
    );
    let third = run(&m, &[]).await.expect("third run");
    assert_eq!(third.emails_destroyed, 1, "{third:?}");
    assert_eq!(emails(&m).await, ["M1"]);
    assert_eq!(keywords(&m, "M1").await, ["$flagged"]);
    assert_eq!(email_state(&m).await, "email-9");
    assert!(problems(&m).await.is_empty(), "{:?}", problems(&m).await);
}

/// `Email/changes` cannot surface the mail already in a mailbox the
/// filter newly admits, so that mailbox is enumerated. An enumeration
/// that failed was once recorded as done all the same, and the mailbox
/// stayed empty.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_newly_admitted_mailbox_is_enumerated_until_an_enumeration_finishes() {
    let m = Mirror::new();
    let tape = Tape::new(&m.playback);
    let a = Account::new(
        &[("MB1", "Inbox"), ("MB2", "Work")],
        &[("M1", &["MB1"]), ("M2", &["MB2"])],
    );
    tape.serve(&a);
    tape.index(&a);
    tape.query(&a, &["MB1"]);
    tape.gets(&a, &["M1"]);
    tape.gets(&a, &["M2"]);
    run(&m, &["Inbox"]).await.expect("first run");
    assert_eq!(emails(&m).await, ["M1"]);
    assert_eq!(listed_whole(&m).await, ["#threads", "MB1"]);

    tape.refuse("Email/query", query_args(0, &["MB2"]), 400);
    run(&m, &["Inbox", "Work"])
        .await
        .expect("an enumeration that fails does not fail the run");
    assert_eq!(problems(&m).await, ["listing:Email/query"]);
    assert_eq!(listed_whole(&m).await, ["#threads", "MB1"]);

    tape.query(&a, &["MB2"]);
    run(&m, &["Inbox", "Work"]).await.expect("third run");
    assert_eq!(emails(&m).await, ["M1", "M2"]);
    assert_eq!(listed_whole(&m).await, ["#threads", "MB1", "MB2"]);
    assert!(problems(&m).await.is_empty(), "{:?}", problems(&m).await);
}

/// A first enumeration that stops part-way once left nothing the next
/// run could use: no state token, so every run began the walk again and
/// fetched every email again. What a walk listed stays listed and is
/// fetched, so the next run fetches only what the rest of the walk adds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_enumeration_cut_short_keeps_what_it_listed() {
    let m = Mirror::new();
    let mut tape = Tape::new(&m.playback);
    tape.query_page = 2;
    let a = Account::new(
        &[("MB1", "Inbox")],
        &[("M1", &["MB1"]), ("M2", &["MB1"]), ("M3", &["MB1"])],
    );
    tape.serve(&a);
    tape.gets(&a, &["M1", "M2"]);
    tape.gets(&a, &["M3"]);
    tape.refuse("Email/query", query_args(2, &[]), 400);
    let first = run(&m, &[])
        .await
        .expect("a walk that stops after listing something does not fail the run");
    assert_eq!(first.emails_upserted, 2, "{first:?}");
    assert_eq!(problems(&m).await, ["listing:Email/query"]);
    assert!(listed_whole(&m).await.is_empty());

    tape.query(&a, &[]);
    let second = run(&m, &[]).await.expect("second run");
    assert_eq!(second.emails_upserted, 1, "{second:?}");
    assert_eq!(emails(&m).await, ["M1", "M2", "M3"]);
    assert_eq!(listed_whole(&m).await, ["*"]);
    assert!(problems(&m).await.is_empty(), "{:?}", problems(&m).await);
}

/// An email that leaves every admitted mailbox is named by the delta,
/// fetched, and found to be outside the filter: its row goes rather
/// than standing as it last was.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_email_moved_out_of_the_admitted_mailboxes_loses_its_row() {
    let m = Mirror::new();
    let tape = Tape::new(&m.playback);
    let mut a = Account::new(
        &[("MB1", "Inbox"), ("MB2", "Work")],
        &[("M1", &["MB1"]), ("M2", &["MB1"])],
    );
    tape.serve(&a);
    tape.query(&a, &["MB1"]);
    run(&m, &["Inbox"]).await.expect("first run");
    assert_eq!(emails(&m).await, ["M1", "M2"]);

    a.email_state = "email-2".into();
    a.emails.get_mut("M1").unwrap().mailboxes = vec!["MB2".into()];
    tape.serve(&a);
    tape.email_changes("email-1", &[], &["M1"], &[], "email-2", false);
    tape.gets(&a, &["M1"]);
    run(&m, &["Inbox"]).await.expect("second run");
    assert_eq!(emails(&m).await, ["M2"]);
}

/// A thread row lists the emails held for it, in the transaction that
/// writes or deletes them: there is no later step for a stop to skip.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_thread_row_is_written_with_its_emails() {
    let m = Mirror::new();
    let tape = Tape::new(&m.playback);
    let mut a = Account::new(&[("MB1", "Inbox")], &[("M1", &["MB1"]), ("M2", &["MB1"])]);
    for email in a.emails.values_mut() {
        email.thread = "T1".into();
    }
    tape.serve(&a);
    run(&m, &[]).await.expect("first run");
    assert_eq!(threads(&m).await, [("T1".to_string(), 2)]);

    a.email_state = "email-2".into();
    a.emails.remove("M2");
    tape.serve(&a);
    tape.email_changes("email-1", &[], &[], &["M2"], "email-2", false);
    run(&m, &[]).await.expect("second run");
    assert_eq!(threads(&m).await, [("T1".to_string(), 1)]);

    a.email_state = "email-3".into();
    a.emails.remove("M1");
    tape.serve(&a);
    tape.email_changes("email-2", &[], &[], &["M1"], "email-3", false);
    run(&m, &[]).await.expect("third run");
    assert!(threads(&m).await.is_empty());
}

// ── helpers ─────────────────────────────────────────────────────────

async fn run(m: &Mirror, labels: &[&str]) -> anyhow::Result<FetchSummary> {
    m.run(|db| {
        let mut opts = FetchOptions::new(db);
        opts.hostname = HOST.to_string();
        opts.only_mailbox_labels = labels.iter().map(|l| l.to_string()).collect();
        datalib_etl_email::ingest::fetch(opts)
    })
    .await
}

async fn strings(m: &Mirror, sql: &'static str) -> Vec<String> {
    m.read(|db: RawDb| async move { sqlx::query_scalar(sql).fetch_all(db.pool()).await.unwrap() })
        .await
}

async fn problems(m: &Mirror) -> Vec<String> {
    strings(m, "SELECT scope_key FROM problems ORDER BY scope_key").await
}

/// Each email's read mark, by id.
async fn marks(m: &Mirror) -> Vec<(&'static str, bool)> {
    let marks = m
        .read(|db: RawDb| async move { read_marks::load(db.pool()).await.unwrap() })
        .await;
    marks
        .into_iter()
        .map(|(id, mark)| {
            let id: &'static str = id.leak();
            match mark {
                ReadMark::Read(read) => (id, read),
                ReadMark::Through(_) => panic!("{id}: an email's mark is Read"),
            }
        })
        .collect()
}

/// Every email row as the store holds it, payload and label ids included.
async fn email_rows(m: &Mirror) -> Vec<String> {
    strings(
        m,
        "SELECT id || ' ' || json(payload) || ' ' || coalesce(label_ids, '-') FROM emails ORDER BY id",
    )
    .await
}

async fn content_commit(m: &Mirror) -> Option<String> {
    m.read(|db: RawDb| async move { dr::content_commit(db.pool()).await.unwrap() })
        .await
}

async fn emails(m: &Mirror) -> Vec<String> {
    strings(m, "SELECT id FROM emails ORDER BY id").await
}

async fn listed_whole(m: &Mirror) -> Vec<String> {
    strings(m, "SELECT scope FROM listed_whole ORDER BY scope").await
}

async fn email_state(m: &Mirror) -> String {
    m.read(|db: RawDb| async move { db.load_state("A1", "Email").await.unwrap().unwrap() })
        .await
}

async fn keywords(m: &Mirror, email_id: &'static str) -> Vec<String> {
    m.read(|db: RawDb| async move {
        sqlx::query_scalar("SELECT keyword FROM email_keywords WHERE email_id = ? ORDER BY keyword")
            .bind(email_id)
            .fetch_all(db.pool())
            .await
            .unwrap()
    })
    .await
}

/// Each thread row and how many emails it lists.
async fn threads(m: &Mirror) -> Vec<(String, i64)> {
    m.read(|db: RawDb| async move {
        sqlx::query_as("SELECT id, email_count FROM threads ORDER BY id")
            .fetch_all(db.pool())
            .await
            .unwrap()
    })
    .await
}

async fn muted(m: &Mirror) -> Vec<i64> {
    m.read(|db: RawDb| async move {
        sqlx::query_scalar("SELECT muted FROM read_marks ORDER BY id")
            .fetch_all(db.pool())
            .await
            .unwrap()
    })
    .await
}
