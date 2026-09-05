// Integration test runs under cargo-test (no MultiProgress / no
// indicatif bars). Exempt from the workspace-wide ban on direct
// stderr/stdout writes defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! Live Gmail REST API download test.

use std::collections::BTreeSet;

use datalib_etl_email::download::gmail_api::{self, FetchOptions};
use datalib_etl_email::download::{db_path_for, RawDb};
use datalib_etl_email_config::EmailGmailApi;

fn test_label() -> String {
    std::env::var("DATALIB_GMAIL_TEST_LABEL").unwrap_or_else(|_| "datalib".to_string())
}

fn opts(root: &std::path::Path, label: &str) -> FetchOptions {
    FetchOptions {
        db_path: root.to_path_buf(),
        config: EmailGmailApi {
            // Leave `account` unset: latchkey resolves the single stored
            // account on its own, and hard-coding an address here would
            // make the test author's mailbox a prerequisite.
            ..Default::default()
        },
        only_labels: vec![label.to_string()],
        ..Default::default()
    }
}

#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn gmail_live_one_label_roundtrip() {
    let label = test_label();
    let tmp = tempfile::TempDir::with_prefix("gmail-live-")
        .expect("create tempdir")
        .keep();
    eprintln!("[test] mirroring label {label:?} into {}", tmp.display());

    // ── run 1: full sync of the label ───────────────────────────────
    let first = gmail_api::fetch(opts(&tmp, &label))
        .await
        .expect("gmail fetch failed — is `latchkey auth browser google-gmail` done?");
    eprintln!("[test] run 1: {first:?}");

    assert!(
        first.full_sync,
        "first run with no cursor must be a full sync"
    );
    assert!(
        first.emails_upserted > 0,
        "label {label:?} produced no messages; pick a label with mail in it",
    );
    assert!(
        !first.budget_exhausted,
        "no budget was set, so the run must not have stopped early",
    );

    let db = RawDb::open(&db_path_for(&tmp)).await.expect("open raw db");

    // ── the label filter actually filtered ──────────────────────────
    let email_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM emails")
        .fetch_one(db.pool())
        .await
        .expect("count emails");
    assert_eq!(
        email_count as usize, first.emails_upserted,
        "every row in the store should be one this run reported writing",
    );

    let mailbox_id = mailbox_id_for(&db, &label).await;
    let labelled: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM email_mailboxes WHERE mailbox_id = ?")
            .bind(&mailbox_id)
            .fetch_one(db.pool())
            .await
            .expect("count labelled");
    assert_eq!(
        labelled, email_count,
        "every mirrored message must carry the label we filtered on",
    );

    // ── every message has its bytes, and they are the bytes claimed ──
    let blob_rows: Vec<(String, Option<String>)> =
        sqlx::query_as("SELECT email_id, blake3 FROM email_blobs")
            .fetch_all(db.pool())
            .await
            .expect("load blob edges");
    assert_eq!(
        blob_rows.len() as i64,
        email_count,
        "every email needs exactly one .eml edge",
    );
    for (email_id, blake3) in &blob_rows {
        let hash = blake3
            .as_deref()
            .unwrap_or_else(|| panic!("email {email_id} has an edge with no blake3"));
        let object = db
            .cas()
            .get(hash)
            .await
            .expect("cas read")
            .unwrap_or_else(|| panic!("email {email_id}: blake3 {hash} is not in the CAS"));
        assert_eq!(
            datalib_etl::blob_cas::blake3_hex(&object.bytes),
            hash,
            "email {email_id}: CAS bytes do not hash to the stored blake3",
        );
        assert_eq!(
            object.byte_len as usize,
            object.bytes.len(),
            "email {email_id}: CAS byte_len disagrees with the bytes",
        );
        // A truncated or mis-alphabet base64url decode still produces
        // bytes; it does not produce something that looks like RFC 5322.
        let head = String::from_utf8_lossy(&object.bytes[..object.bytes.len().min(2048)]);
        assert!(
            head.contains('\n'),
            "email {email_id}: .eml has no line breaks — the decode is suspect",
        );
        assert!(
            head.to_ascii_lowercase().contains("message-id:")
                || head.to_ascii_lowercase().contains("from:"),
            "email {email_id}: .eml has no recognizable headers — the decode is suspect",
        );
    }

    // ── threads list the messages they contain ──────────────────────
    let emails: Vec<(String, String)> = sqlx::query_as("SELECT id, thread_id FROM emails")
        .fetch_all(db.pool())
        .await
        .expect("load emails");
    for (email_id, thread_id) in &emails {
        // `json(payload)`: the column stores a JSONB blob, so reading it
        // straight would come back as BLOB. Same wrapper
        // `doltlite_raw::load_payloads` uses.
        let payload: String = sqlx::query_scalar("SELECT json(payload) FROM threads WHERE id = ?")
            .bind(thread_id)
            .fetch_one(db.pool())
            .await
            .unwrap_or_else(|e| panic!("email {email_id} has no thread row {thread_id}: {e}"));
        let members: BTreeSet<String> = serde_json::from_str::<serde_json::Value>(&payload)
            .expect("thread payload is json")
            .get("emailIds")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            members.contains(email_id),
            "thread {thread_id} does not list its own message {email_id}",
        );
    }

    // ── the gmail id mapping exists for every row ───────────────────
    let mapped: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM gmail_messages")
        .fetch_one(db.pool())
        .await
        .expect("count gmail_messages");
    assert_eq!(
        mapped, email_count,
        "every row needs its Gmail-id mapping, or deletions can't find it",
    );

    drop(db);

    // ── run 2: incremental, and a no-op ─────────────────────────────
    let second = gmail_api::fetch(opts(&tmp, &label))
        .await
        .expect("second gmail fetch failed");
    eprintln!("[test] run 2: {second:?}");

    assert!(
        !second.full_sync,
        "second run should resume from the stored historyId, not re-enumerate",
    );
    assert_eq!(
        second.emails_upserted, 0,
        "nothing changed upstream, so the second run should write nothing",
    );
    assert_eq!(
        second.blobs_stored, 0,
        "the second run must not re-store any .eml bytes",
    );
    // `messages.get` is 20 units; the incremental path should be a
    // profile + labels + history call and nothing else.
    assert!(
        second.quota_units_spent < u64::from(gmail_api::api::UNITS_MESSAGES_GET),
        "second run spent {} quota units — it fetched message bodies it already had",
        second.quota_units_spent,
    );

    let db = RawDb::open(&db_path_for(&tmp))
        .await
        .expect("reopen raw db");
    let after: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM emails")
        .fetch_one(db.pool())
        .await
        .expect("recount emails");
    assert_eq!(after, email_count, "the second run changed the row count");

    eprintln!("[test] ok: {email_count} messages under {label:?}, second run was a no-op");
}

async fn mailbox_id_for(db: &RawDb, label: &str) -> String {
    let rows: Vec<(String, Option<String>)> = sqlx::query_as("SELECT id, name FROM mailboxes")
        .fetch_all(db.pool())
        .await
        .expect("load mailboxes");
    rows.into_iter()
        .find(|(_, name)| name.as_deref() == Some(label))
        .unwrap_or_else(|| {
            panic!("no mailbox row named {label:?} — the label filter resolved to something else")
        })
        .0
}

/// A budget-limited backfill must **walk forward** across runs.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn gmail_live_budget_limited_backfill_makes_progress() {
    const BUDGET: usize = 2;
    const MAX_RUNS: usize = 20;

    let label = test_label();
    let tmp = tempfile::TempDir::with_prefix("gmail-live-budget-")
        .expect("create tempdir")
        .keep();

    // What the whole label holds, so we know what "done" means.
    let total = {
        let probe = tempfile::TempDir::with_prefix("gmail-live-probe-")
            .expect("create tempdir")
            .keep();
        gmail_api::fetch(opts(&probe, &label))
            .await
            .expect("probe fetch failed")
            .emails_upserted
    };
    assert!(
        total > BUDGET,
        "label {label:?} has {total} messages; need more than the {BUDGET}-message \
         budget for this test to exercise anything",
    );

    let mut runs = 0;
    let mut written = 0;
    loop {
        let mut o = opts(&tmp, &label);
        o.config.message_budget = Some(BUDGET);
        let s = gmail_api::fetch(o).await.expect("budgeted fetch failed");
        runs += 1;
        eprintln!(
            "[test] budget run {runs}: +{} emails, exhausted={}, full_sync={}, units={}",
            s.emails_upserted, s.budget_exhausted, s.full_sync, s.quota_units_spent,
        );

        assert!(
            s.emails_upserted > 0 || !s.budget_exhausted,
            "run {runs} wrote nothing but claims more to do — the backfill is stuck, \
             which is exactly the bug this test exists for",
        );
        assert!(
            s.emails_upserted <= BUDGET,
            "run {runs} wrote {} messages against a budget of {BUDGET}",
            s.emails_upserted,
        );
        written += s.emails_upserted;

        if !s.budget_exhausted {
            // The run that finishes the backfill is the one that may
            // store the cursor.
            break;
        }
        assert!(
            runs < MAX_RUNS,
            "still not done after {MAX_RUNS} runs of {BUDGET} — not converging",
        );
    }

    assert_eq!(
        written, total,
        "budgeted runs mirrored {written} of {total} messages; the tail was abandoned",
    );

    // And now that it has caught up, it should go incremental.
    let after = gmail_api::fetch(opts(&tmp, &label))
        .await
        .expect("post-backfill fetch failed");
    assert!(
        !after.full_sync,
        "once the backfill completes, the cursor should be stored and the next run \
         should resume incrementally",
    );
    assert_eq!(after.emails_upserted, 0, "the catch-up run wrote new rows");

    eprintln!("[test] ok: {total} messages backfilled over {runs} runs of {BUDGET}");
}
