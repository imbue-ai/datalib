// Integration test runs under cargo-test (no MultiProgress / no
// indicatif bars). Exempt from the workspace-wide ban on direct
// stderr/stdout writes defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! Live Google Contacts download, the check of the shapes `INGEST.md` gives.
//!
//! Not run by `bazel test`: every test here is named `live::<fn>`, and
//! the target skips that prefix. `bazel run
//! //datalib/backend/etl/providers/contacts:contacts_live` runs exactly
//! these, against the login latchkey holds for `google-people`
//! (`GOOGLE_PEOPLE_TEST_ACCOUNT` names the latchkey account when there
//! are several). It reads, and writes nothing upstream. The store is left
//! on disk, and its path printed, for inspection; nothing it holds is
//! printed.

use std::collections::HashSet;

use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_contacts::ingest::google;
use datalib_etl_contacts::ingest::{db_path_for, FetchSummary, RawDb};
use datalib_etl_web::http::LatchkeySettings;

async fn run(store: &std::path::Path) -> FetchSummary {
    let db = RawDb::open(&db_path_for(store)).await.unwrap();
    let r = google::fetch(google::FetchOptions {
        db: db.clone(),
        labels: Vec::new(),
        blob_size_limit_bytes: None,
        latchkey: LatchkeySettings {
            account: std::env::var("GOOGLE_PEOPLE_TEST_ACCOUNT").ok(),
        },
        progress: Default::default(),
        control: Default::default(),
        sealer: None,
    })
    .await;
    db.commit_all("contacts_live")
        .await
        .expect("seal the raw store");
    db.close().await;
    r.expect("google contacts fetch")
}

async fn count(store: &std::path::Path, sql: &'static str) -> i64 {
    let db = RawDb::open(&db_path_for(store)).await.unwrap();
    let n = sqlx::query_scalar(sql).fetch_one(db.pool()).await.unwrap();
    db.close().await;
    n
}

/// Every contact, label and photo lands, the replies carried what the
/// reference says they do, and a second run with nothing changed in
/// between asks for nothing it holds. Listing everyone again, as an
/// expired token does, changes nothing render reads.
#[tokio::test(flavor = "multi_thread")]
async fn contacts_labels_and_photos_land_and_a_second_run_owes_nothing() {
    let store = tempfile::TempDir::with_prefix("google-contacts-live-")
        .unwrap()
        .keep();
    eprintln!("[test] store: {}", store.display());

    let first = run(&store).await;
    eprintln!("[test] first run: {first:?}");
    assert_eq!(first.errors, 0, "{first:?}");
    let people = count(&store, "SELECT count(*) FROM google_people").await;
    assert_eq!(people as usize, first.contacts_new);
    let unnamed = count(
        &store,
        "SELECT count(*) FROM google_people, json_each(json_extract(payload, '$.memberships')) m \
         WHERE json_extract(m.value, '$.contactGroupMembership') IS NOT NULL \
         AND json_extract(m.value, '$.contactGroupMembership.contactGroupResourceName') \
         NOT IN (SELECT resource_name FROM google_contact_groups)",
    )
    .await;
    assert_eq!(
        unnamed, 0,
        "a membership of a label the listing did not name"
    );
    let missing = count(
        &store,
        "SELECT count(*) FROM google_photos WHERE blake3 IS NULL",
    )
    .await;
    assert_eq!(missing, 0, "a listed photo that was not fetched");

    let second = run(&store).await;
    eprintln!("[test] second run: {second:?}");
    assert_eq!(
        (
            second.contacts_new,
            second.contacts_updated,
            second.contacts_deleted,
            second.photos,
            second.errors,
            second.requests
        ),
        (0, 0, 0, 0, 0, 3),
        "the account, the labels and one page: {second:?}"
    );

    let rendered = render_reads(&store).await;
    let db = RawDb::open(&db_path_for(&store)).await.unwrap();
    sqlx::query("DELETE FROM sync_scope_state")
        .execute(db.pool())
        .await
        .unwrap();
    db.commit_all("forget the sync token").await.unwrap();
    db.close().await;
    let third = run(&store).await;
    eprintln!("[test] a whole listing again: {third:?}");
    assert_eq!(
        (third.contacts_updated, third.contacts_deleted, third.errors),
        (0, 0, 0),
        "{third:?}"
    );
    let moved = rendered
        .symmetric_difference(&render_reads(&store).await)
        .count();
    assert_eq!(moved, 0, "rows render reads that a second listing changed");
}

async fn render_reads(store: &std::path::Path) -> HashSet<String> {
    let db = RawDb::open(&db_path_for(store)).await.unwrap();
    let rows: Vec<String> = sqlx::query_scalar(
        "SELECT id || json(payload) FROM google_people \
         UNION ALL SELECT id || json(payload) FROM google_contact_groups \
         UNION ALL SELECT id || blake3 FROM google_photos",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    db.close().await;
    rows.into_iter().collect()
}
