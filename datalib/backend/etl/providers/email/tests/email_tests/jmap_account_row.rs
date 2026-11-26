//! Fastmail (JMAP): the account row names the address the credentials
//! sign in as, in its `email_address` column. JMAP's account object
//! carries no address, so the column was always empty (#1365).

use datalib_etl_email::ingest::{fetch, FetchOptions, RawDb};

use crate::jmap_tape::{Account, Tape, HOST};
use crate::support::Mirror;
use datalib_sql::test_sql;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_personal_account_row_carries_the_sign_ins_address() {
    let m = Mirror::new();
    let tape = Tape::new(&m.playback);
    tape.serve(&Account::new(&[("MB1", "Inbox")], &[("M1", &["MB1"])]));
    m.run(|db| {
        let mut opts = FetchOptions::new(db);
        opts.hostname = HOST.to_string();
        fetch(opts)
    })
    .await
    .expect("one run");

    let address: Option<String> = m
        .read(|db: RawDb| async move {
            test_sql!("SELECT email_address FROM accounts")
                .query_scalar()
                .fetch_one(db.pool())
                .await
                .unwrap()
        })
        .await;
    assert_eq!(address.as_deref(), Some("t@example.test"));
}
