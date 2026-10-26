//! Fastmail (JMAP): a label filter keeps whole threads. The start of a
//! conversation filed outside the admitted mailboxes is listed from the
//! thread index and fetched with the reply the filter keeps; a thread no
//! kept email is in any more goes whole; and with `whole_threads = false`
//! the thread row says how many emails it has upstream.

use datalib_etl_email::ingest::{FetchOptions, FetchSummary, RawDb};

use crate::jmap_tape::{Account, Tape, HOST};
use crate::support::Mirror;

/// `M1`, archived, starts the thread; `M2`, its reply, is in the inbox.
fn account() -> Account {
    let mut a = Account::new(
        &[("MB1", "Inbox"), ("MB2", "Archive")],
        &[("M1", &["MB2"]), ("M2", &["MB1"])],
    );
    a.emails.get_mut("M2").unwrap().thread = "TM1".into();
    a
}

fn serve(tape: &Tape, a: &Account) {
    tape.serve(a);
    tape.index(a);
    tape.query(a, &["MB1"]);
    tape.gets(a, &["M1"]);
    tape.gets(a, &["M2"]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_rest_of_a_kept_emails_thread_is_mirrored() {
    let m = Mirror::new();
    let tape = Tape::new(&m.playback);
    serve(&tape, &account());
    let run = run(&m, true).await.expect("first run");
    assert_eq!(run.listed_from_threads, 1, "{run:?}");
    assert_eq!(emails(&m).await, ["M1", "M2"]);
    assert_eq!(threads(&m).await, [(2, Some(2))]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn without_whole_threads_a_partial_thread_shows_its_count() {
    let m = Mirror::new();
    let tape = Tape::new(&m.playback);
    serve(&tape, &account());
    run(&m, false).await.expect("first run");
    assert_eq!(emails(&m).await, ["M2"]);
    assert_eq!(threads(&m).await, [(1, Some(2))]);
}

/// The reply is archived too: no email of the thread is in the inbox
/// any more, so the thread goes, the start with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_thread_the_filter_no_longer_keeps_goes_whole() {
    let m = Mirror::new();
    let tape = Tape::new(&m.playback);
    let mut a = account();
    serve(&tape, &a);
    run(&m, true).await.expect("first run");

    a.email_state = "email-2".into();
    a.emails.get_mut("M2").unwrap().mailboxes = vec!["MB2".into()];
    serve(&tape, &a);
    tape.email_changes("email-1", &[], &["M2"], &[], "email-2", false);
    let run = run(&m, true).await.expect("second run");
    assert_eq!(run.emails_destroyed, 2, "{run:?}");
    assert!(emails(&m).await.is_empty());
    assert!(threads(&m).await.is_empty());
}

async fn run(m: &Mirror, whole_threads: bool) -> anyhow::Result<FetchSummary> {
    m.run(|db| {
        let mut opts = FetchOptions::new(db);
        opts.hostname = HOST.to_string();
        opts.only_mailbox_labels = vec!["Inbox".to_string()];
        opts.whole_threads = whole_threads;
        datalib_etl_email::ingest::fetch(opts)
    })
    .await
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

/// Each thread's `(email_count, upstream_email_count)`.
async fn threads(m: &Mirror) -> Vec<(i64, Option<i64>)> {
    m.read(|db: RawDb| async move {
        sqlx::query_as("SELECT email_count, upstream_email_count FROM threads")
            .fetch_all(db.pool())
            .await
            .unwrap()
    })
    .await
}
