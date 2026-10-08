//! When part of a Fastmail (JMAP) sync fails: a listing that did not
//! answer is a `problems` row and the rest of the run goes on; a
//! download the server refused ends the `.eml` phase, keeping what
//! landed; and each row clears only once the thing it is about is tried
//! again and works.

use datalib_etl_email::ingest::{FetchOptions, FetchSummary, RawDb};

use crate::jmap_tape::{mailbox_get_args, query_args, status, Account, Tape, HOST};
use crate::support::Mirror;

/// A full re-list whose `Mailbox/get` fails keeps the mailboxes an
/// earlier run stored and still mirrors the mail.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_mailbox_listing_that_fails_is_a_row_and_the_run_goes_on() {
    let m = Mirror::new();
    let tape = Tape::new(&m.playback);
    tape.serve(&Account::new(&[("MB1", "Inbox")], &[("M1", &["MB1"])]));
    run(&m, |_| {}).await.expect("first run");

    tape.serve(&Account::new(
        &[("MB1", "Inbox")],
        &[("M1", &["MB1"]), ("M2", &["MB1"])],
    ));
    tape.refuse("Mailbox/get", mailbox_get_args(None), 400);
    let second = run(&m, |_| {})
        .await
        .expect("a mailbox listing that fails does not fail the run");
    assert_eq!(second.emails_upserted, 2, "{second:?}");
    assert_eq!(problems(&m).await, [row("listing:Mailbox/get", "error")]);
    assert_eq!(mailboxes(&m).await, ["MB1"]);

    tape.serve(&Account::new(
        &[("MB1", "Inbox")],
        &[("M1", &["MB1"]), ("M2", &["MB1"])],
    ));
    run(&m, |_| {}).await.expect("third run");
    assert!(
        problems(&m).await.is_empty(),
        "listing again clears the row"
    );
}

/// An `Email/query` walk that fails keeps every stored email: absence
/// from a walk that stopped means nothing. With nothing stored, there is
/// nothing to keep going for.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_email_listing_that_fails_deletes_nothing() {
    let m = Mirror::new();
    let tape = Tape::new(&m.playback);
    tape.serve(&Account::new(
        &[("MB1", "Inbox")],
        &[("M1", &["MB1"]), ("M2", &["MB1"])],
    ));
    run(&m, |_| {}).await.expect("first run");

    tape.refuse("Email/query", query_args(0, &[]), 400);
    run(&m, |_| {})
        .await
        .expect("an email listing that fails does not fail the run");
    assert_eq!(problems(&m).await, [row("listing:Email/query", "error")]);
    assert_eq!(emails(&m).await, ["M1", "M2"]);

    let fresh = Mirror::new();
    let tape = Tape::new(&fresh.playback);
    tape.serve(&Account::new(&[("MB1", "Inbox")], &[]));
    tape.refuse("Email/query", query_args(0, &[]), 400);
    run(&fresh, |_| {})
        .await
        .expect_err("with no email stored, a listing that fails fails the run");
}

/// A refused credential fails every download after it the same way, so
/// it ends the phase rather than writing one failure per `.eml`. The
/// refusal was once the run's error, and a failed run commits nothing:
/// every body the run had downloaded went with it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_refused_download_stops_the_phase_and_keeps_what_downloaded() {
    let m = Mirror::new();
    let tape = Tape::new(&m.playback);
    let mail: [(&str, &[&str]); 3] = [("M1", &["MB1"]), ("M2", &["MB1"]), ("M3", &["MB1"])];
    tape.serve(&Account::new(&[("MB1", "Inbox")], &mail));
    tape.blob("M2", status(401));
    let first = run(&m, |o| o.blob_download_concurrency = Some(1))
        .await
        .expect("a refused download keeps the run's work");
    assert_eq!(first.blobs_downloaded, 1, "{first:?}");
    assert_eq!(blobs(&m).await, [("M1".to_string(), true)]);
    assert_eq!(problems(&m).await, [row("phase:eml_download", "error")]);
    let said = sample(&m, "phase:eml_download").await;
    // The cause leads, since the sample is eighty characters; the URL
    // in it takes them all, so the status is past the cut.
    assert!(said.starts_with("JMAP download "), "{said}");

    tape.serve(&Account::new(&[("MB1", "Inbox")], &mail));
    let second = run(&m, |_| {}).await.expect("second run");
    assert_eq!(second.blobs_downloaded, 2, "{second:?}");
    assert_eq!(
        blobs(&m).await,
        ["M1", "M2", "M3"].map(|id| (id.to_string(), true))
    );
    assert!(problems(&m).await.is_empty(), "{:?}", problems(&m).await);
}

/// Twenty downloads failing in a row end the phase the same way: the
/// bodies before them are kept, each failure keeps its own row, and the
/// bodies the phase never asked for are downloaded by the next run.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_run_of_failed_downloads_stops_the_phase_and_keeps_what_downloaded() {
    let m = Mirror::new();
    let tape = Tape::new(&m.playback);
    let ids: Vec<String> = (1..=25).map(|n| format!("M{n:02}")).collect();
    let mail: Vec<(&str, &[&str])> = ids.iter().map(|id| (id.as_str(), &["MB1"][..])).collect();
    tape.serve(&Account::new(&[("MB1", "Inbox")], &mail));
    for id in &ids[3..23] {
        tape.blob(id, status(404));
    }
    let first = run(&m, |o| o.blob_download_concurrency = Some(1))
        .await
        .expect("a spent failure budget keeps the run's work");
    assert_eq!(first.blobs_downloaded, 3, "{first:?}");
    let stored = blobs(&m).await;
    assert_eq!(
        stored.iter().filter(|(_, landed)| *landed).count(),
        3,
        "{stored:?}"
    );
    assert_eq!(stored.len(), 23, "the last two were never asked for");
    let found = problems(&m).await;
    assert!(
        found.contains(&row("phase:eml_download", "error")),
        "{found:?}"
    );
    assert_eq!(found.len(), 21, "one row per failed .eml, and the phase's");
    let said = sample(&m, "phase:eml_download").await;
    assert!(
        said.starts_with("JMAP download "),
        "the last failure leads: {said}"
    );

    tape.serve(&Account::new(&[("MB1", "Inbox")], &mail));
    let second = run(&m, |_| {}).await.expect("second run");
    assert_eq!(second.blobs_downloaded, 22, "{second:?}");
    assert!(blobs(&m).await.iter().all(|(_, landed)| *landed));
    assert!(problems(&m).await.is_empty(), "{:?}", problems(&m).await);
}

/// Every body once waited in memory for one write at the end of the
/// phase, so a kill lost them all and no seal could publish any. With
/// the bound at one byte each body is its own write, and each write is
/// a point the run may seal at: some commit holds part of the bodies.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn bodies_are_written_and_sealed_as_they_land() {
    let m = Mirror::new();
    let tape = Tape::new(&m.playback);
    tape.serve(&Account::new(
        &[("MB1", "Inbox")],
        &[("M1", &["MB1"]), ("M2", &["MB1"]), ("M3", &["MB1"])],
    ));
    m.run_sealing(|db, sealer| {
        let mut opts = FetchOptions::new(db);
        opts.hostname = HOST.to_string();
        opts.full_resync = true;
        opts.sealer = Some(sealer);
        opts.blob_download_concurrency = Some(1);
        opts.blob_flush_count = Some(1);
        datalib_etl_email::ingest::fetch(opts)
    })
    .await
    .expect("run");

    let held = m
        .read(|db: RawDb| async move {
            let commits: Vec<String> = sqlx::query_scalar("SELECT commit_hash FROM dolt_log")
                .fetch_all(db.pool())
                .await
                .unwrap();
            let mut held = Vec::new();
            for commit in commits {
                // Audited for `AssertSqlSafe`: `commit` is a hash doltlite
                // just listed, and the table function takes a literal. The
                // store's first commit predates the table.
                let n: i64 = sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
                    "SELECT count(*) FROM dolt_at_email_blobs('{commit}') \
                     WHERE blake3 IS NOT NULL"
                )))
                .fetch_one(db.pool())
                .await
                .unwrap_or(0);
                held.push(n);
            }
            held
        })
        .await;
    assert!(
        held.contains(&3),
        "the run's end holds every body: {held:?}"
    );
    assert!(
        held.contains(&1) && held.contains(&2),
        "no commit holds only part of the bodies, so none was sealed mid-phase: {held:?}"
    );
}

/// An `.eml` over `blob_size_limit_bytes` was turned away on purpose: a
/// warning that the mirror lacks it, not a download that failed.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_oversize_eml_is_a_skip_not_a_failure() {
    let m = Mirror::new();
    let tape = Tape::new(&m.playback);
    tape.serve(&Account::new(&[("MB1", "Inbox")], &[("M1", &["MB1"])]));
    run(&m, |o| o.blob_size_limit_bytes = Some(10))
        .await
        .expect("run");
    let rows = m
        .read(|db: RawDb| async move {
            sqlx::query_as::<_, (String, String, String)>(
                "SELECT scope_key, severity, reason FROM problems",
            )
            .fetch_all(db.pool())
            .await
            .unwrap()
        })
        .await;
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert!(rows[0].0.starts_with("email_blobs:M1#"), "{rows:?}");
    assert_eq!(
        (rows[0].1.as_str(), rows[0].2.as_str()),
        ("warning", "over_size_limit")
    );
}

/// A label path that matched nothing is a row, and the run that no
/// longer names it clears the row, filter or no filter.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_unmatched_label_row_goes_when_the_filter_does() {
    let m = Mirror::new();
    let tape = Tape::new(&m.playback);
    tape.serve(&Account::new(&[("MB1", "Inbox")], &[("M1", &["MB1"])]));
    run(&m, |o| o.only_mailbox_labels = vec!["Starbase".into()])
        .await
        .expect("first run");
    assert_eq!(
        problems(&m).await,
        [row("config:only_extract_labels:Starbase", "warning")]
    );

    run(&m, |_| {}).await.expect("second run");
    assert!(problems(&m).await.is_empty());
}

// ── helpers ─────────────────────────────────────────────────────────

/// A full re-list, as the tests above mostly want, unless `tweak` says
/// otherwise.
async fn run(m: &Mirror, tweak: impl FnOnce(&mut FetchOptions)) -> anyhow::Result<FetchSummary> {
    m.run(|db| {
        let mut opts = FetchOptions::new(db);
        opts.hostname = HOST.to_string();
        opts.full_resync = true;
        tweak(&mut opts);
        datalib_etl_email::ingest::fetch(opts)
    })
    .await
}

fn row(key: &str, severity: &str) -> (String, String) {
    (key.to_string(), severity.to_string())
}

async fn problems(m: &Mirror) -> Vec<(String, String)> {
    m.read(|db: RawDb| async move {
        sqlx::query_as("SELECT scope_key, severity FROM problems ORDER BY scope_key")
            .fetch_all(db.pool())
            .await
            .unwrap()
    })
    .await
}

async fn sample(m: &Mirror, key: &'static str) -> String {
    m.read(|db: RawDb| async move {
        sqlx::query_scalar("SELECT sample FROM problems WHERE scope_key = ?")
            .bind(key)
            .fetch_one(db.pool())
            .await
            .unwrap()
    })
    .await
}

async fn ids(m: &Mirror, sql: &'static str) -> Vec<String> {
    m.read(|db: RawDb| async move { sqlx::query_scalar(sql).fetch_all(db.pool()).await.unwrap() })
        .await
}

async fn mailboxes(m: &Mirror) -> Vec<String> {
    ids(m, "SELECT id FROM mailboxes ORDER BY id").await
}

async fn emails(m: &Mirror) -> Vec<String> {
    ids(m, "SELECT id FROM emails ORDER BY id").await
}

/// Each `email_blobs` edge: its email, and whether its bytes landed.
async fn blobs(m: &Mirror) -> Vec<(String, bool)> {
    m.read(|db: RawDb| async move {
        sqlx::query_as("SELECT email_id, blake3 IS NOT NULL FROM email_blobs ORDER BY email_id")
            .fetch_all(db.pool())
            .await
            .unwrap()
    })
    .await
}
