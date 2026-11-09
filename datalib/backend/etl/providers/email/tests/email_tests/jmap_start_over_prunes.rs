//! A Fastmail (JMAP) state the server can no longer replay from lists
//! the account again, and that is the one moment the mirror can see what
//! was destroyed while no state was replaying: a mailbox the full
//! `Mailbox/get` does not name comes off every email and goes, and an
//! email the finished, unfiltered `Email/query` does not name goes.

use datalib_etl_email::ingest::{FetchOptions, FetchSummary, RawDb};

use crate::jmap_tape::{email_get_args, Account, Tape, ACCOUNT, HOST};
use crate::support::Mirror;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_over_drops_what_upstream_destroyed() {
    let m = Mirror::new();
    let tape = Tape::new(&m.playback);
    tape.serve(&Account::new(
        &[("MB1", "Inbox"), ("MB2", "Work")],
        &[("M1", &["MB1", "MB2"]), ("M2", &["MB2"])],
    ));
    let first = run(&m).await;
    assert_eq!(first.emails_upserted, 2, "{first:?}");

    // `Work` was destroyed, and `M2` with it; `M1` is only in the Inbox.
    tape.serve_aged_out(&Account::new(&[("MB1", "Inbox")], &[("M1", &["MB1"])]));
    let second = run(&m).await;
    assert_eq!(second.mailboxes_destroyed, 1, "{second:?}");
    assert_eq!(second.emails_destroyed, 1, "{second:?}");

    let (mailboxes, emails, joins) = m
        .read(|db: RawDb| async move {
            let mailboxes: Vec<String> = db
                .mailbox_names(ACCOUNT)
                .await
                .unwrap()
                .into_keys()
                .collect();
            let emails: Vec<String> = sqlx::query_scalar("SELECT id FROM emails ORDER BY id")
                .fetch_all(db.pool())
                .await
                .unwrap();
            let joins = db.load_email_joins().await.unwrap().mailboxes;
            (mailboxes, emails, joins)
        })
        .await;
    assert_eq!(mailboxes, vec!["MB1"]);
    assert_eq!(emails, vec!["M1"]);
    assert_eq!(joins["M1"], vec!["MB1"]);
}

/// Listing an account again fetches every email, and each request names
/// as many as the server's `maxObjectsInGet` allows. At a fixed 50, a
/// 277k-email Fastmail account took an hour of requests. The one request
/// naming all three emails is refused, so a run that ignores the limit
/// fetches nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_start_over_asks_as_many_emails_at_once_as_the_server_allows() {
    let m = Mirror::new();
    let tape = Tape::with_max_objects_in_get(&m.playback, 2);
    let a = Account::new(
        &[("MB1", "Inbox")],
        &[("M1", &["MB1"]), ("M2", &["MB1"]), ("M3", &["MB1"])],
    );
    let in_batches_of_two = |tape: &Tape| {
        tape.refuse("Email/get", email_get_args(&["M1", "M2", "M3"]), 400);
        tape.gets(&a, &["M1", "M2"]);
        tape.gets(&a, &["M3"]);
    };
    tape.serve(&a);
    in_batches_of_two(&tape);
    let first = run(&m).await;
    assert_eq!(first.emails_upserted, 3, "{first:?}");

    tape.serve_aged_out(&a);
    in_batches_of_two(&tape);
    let second = run(&m).await;
    assert_eq!(second.emails_upserted, 3, "{second:?}");
}

async fn run(m: &Mirror) -> FetchSummary {
    m.run(|db| {
        let mut opts = FetchOptions::new(db);
        opts.hostname = HOST.to_string();
        datalib_etl_email::ingest::fetch(opts)
    })
    .await
    .expect("jmap fetch under playback")
}
