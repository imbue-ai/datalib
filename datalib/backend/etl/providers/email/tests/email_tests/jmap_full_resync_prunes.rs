//! A full Fastmail (JMAP) re-list is the one moment the mirror can see
//! what was destroyed while no cursor was replaying: a mailbox the full
//! `Mailbox/get` does not name comes off every email and goes, and an
//! email the finished, unfiltered `Email/query` does not name goes.

use datalib_etl_email::ingest::{FetchOptions, FetchSummary, RawDb};

use crate::jmap_tape::{Account, Tape, ACCOUNT, HOST};
use crate::support::Mirror;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_full_resync_drops_what_upstream_destroyed() {
    let m = Mirror::new();
    write_run(
        &m.playback,
        &[("MB1", "Inbox"), ("MB2", "Work")],
        &[("M1", &["MB1", "MB2"]), ("M2", &["MB2"])],
    );
    let first = run(&m).await;
    assert_eq!(first.emails_upserted, 2, "{first:?}");

    // `Work` was destroyed, and `M2` with it; `M1` is only in the Inbox.
    write_run(&m.playback, &[("MB1", "Inbox")], &[("M1", &["MB1"])]);
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

async fn run(m: &Mirror) -> FetchSummary {
    m.run(|db| {
        let mut opts = FetchOptions::new(db);
        opts.hostname = HOST.to_string();
        opts.full_resync = true;
        datalib_etl_email::ingest::fetch(opts)
    })
    .await
    .expect("jmap fetch under playback")
}

fn write_run(out: &std::path::Path, mailboxes: &[(&str, &str)], emails: &[(&str, &[&str])]) {
    Tape::new(out).serve(&Account::new(mailboxes, emails));
}
