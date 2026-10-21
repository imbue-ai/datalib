// Integration test runs under cargo-test (no MultiProgress / no
// indicatif bars). Exempt from the workspace-wide ban on direct
// stderr/stdout writes defined in clippy.toml.
#![allow(clippy::disallowed_macros)]

//! Live Google Drive download of a few named files.
//!
//! Not run by `bazel test`: every test here is named `live::<fn>`, and
//! the target skips that prefix.
//! `GOOGLE_DRIVE_TEST_FILE_IDS=<id>,<id> bazel run
//! //datalib/backend/etl/providers/google_drive:google_drive_live` runs
//! exactly these, against the account latchkey holds for
//! `google-drive`, `google-docs`, `google-sheets` and `google-slides`.
//! The store is left on disk, and its path printed, for inspection.

use datalib_etl_google_drive::ingest::{self, db_path_for, FetchSummary, RawDb};
use datalib_etl_google_drive_config::GoogleDriveApi;
use datalib_etl_web::http::LatchkeySettings;

async fn run(store: &std::path::Path, api: &GoogleDriveApi) -> FetchSummary {
    let db = RawDb::open(&db_path_for(store)).await.unwrap();
    let r = ingest::fetch(ingest::FetchOptions {
        db: db.clone(),
        latchkey: LatchkeySettings {
            account: std::env::var("GOOGLE_DRIVE_TEST_ACCOUNT").ok(),
        },
        api: api.clone(),
        progress: Default::default(),
        control: Default::default(),
        sealer: None,
    })
    .await;
    let sealed = datalib_etl::doltlite_raw::commit_run(db.pool(), "google_drive_live").await;
    db.close().await;
    sealed.expect("seal the raw store");
    r.expect("google_drive fetch")
}

#[tokio::test(flavor = "multi_thread")]
async fn named_files_capture_whole_and_a_second_run_owes_nothing() {
    let ids: Vec<String> = std::env::var("GOOGLE_DRIVE_TEST_FILE_IDS")
        .expect("set GOOGLE_DRIVE_TEST_FILE_IDS to a comma-separated list of Drive file ids")
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    let api = GoogleDriveApi {
        file_ids: ids.clone(),
        ..Default::default()
    };
    let store = tempfile::TempDir::with_prefix("google-drive-live-")
        .unwrap()
        .keep();
    eprintln!("[test] store: {}", store.display());

    let first = run(&store, &api).await;
    eprintln!("[test] first run: {}", first.line());
    assert_eq!(first.listed, ids.len(), "{first:?}");
    assert_eq!(first.captured, ids.len(), "{first:?}");

    let db = RawDb::open(&db_path_for(&store)).await.unwrap();
    let without_native: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM captures c WHERE NOT EXISTS \
         (SELECT 1 FROM capture_parts p WHERE p.capture_id = c.id \
          AND p.part = 'native.json.gz' AND p.blake3 IS NOT NULL)",
    )
    .fetch_one(db.pool())
    .await
    .unwrap();
    assert_eq!(without_native, 0, "every capture holds its native JSON");
    db.close().await;

    // Nothing was edited in between, so every file is held at the
    // version Drive lists now.
    let second = run(&store, &api).await;
    eprintln!("[test] second run: {}", second.line());
    assert_eq!(second.owed, 0, "{second:?}");
}
