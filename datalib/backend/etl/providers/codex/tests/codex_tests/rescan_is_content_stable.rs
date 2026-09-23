//! Re-reading one Codex home must move no content table. The bug this
//! guards is a stamp the store itself mints landing in a content row:
//! the live bake found exactly that on `pdf`, `media` and `fsindex`
//! (imbue-ai/datalib#671), where every row then read as modified on
//! every sync and the render re-rendered everything.
//!
//! Two conditions have to hold at once for such a stamp to show, and
//! leaving out either makes this test pass against the bug. The clocks
//! must differ — one pinned `now` is what hid it on the others — and
//! the files must actually be **re-read**, which for this provider
//! means clearing the cursor first: a rollout whose bytes have not
//! moved is never opened again, so a second plain scan writes nothing
//! and would prove nothing.

use std::path::PathBuf;

use datalib_etl::control::DownloadControl;
use datalib_etl::fingerprint_cache::FingerprintCache;
use datalib_etl::progress::Progress;
use datalib_etl_codex::ingest::{db_path_for, fetch, FetchOptions, RawDb};

fn fixture_dir() -> PathBuf {
    if let Ok(d) = std::env::var("CODEX_FIXTURE_DIR") {
        return PathBuf::from(d);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/codex_tng")
}

async fn scan_and_commit(raw_path: &std::path::Path, cache: &FingerprintCache) -> String {
    let db = RawDb::open(&db_path_for(raw_path)).await.unwrap();
    fetch(FetchOptions {
        db: db.clone(),
        input_path: fixture_dir(),
        cache: cache.clone(),
        progress: Progress::default(),
        control: DownloadControl::default(),
    })
    .await
    .unwrap();
    datalib_etl::doltlite_raw::commit_run(db.pool(), "scan")
        .await
        .ok();
    let head = datalib_etl::doltlite_raw::head_commit(db.pool())
        .await
        .unwrap()
        .expect("the scan committed");
    db.close().await;
    head
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_scan_of_one_home_moves_no_content_table() {
    let td = tempfile::tempdir().unwrap();
    let raw_path = td.path().join("raw");
    std::fs::create_dir_all(&raw_path).unwrap();
    let cache = FingerprintCache::open(&td.path().join("fp.sqlite"))
        .await
        .unwrap();

    std::env::set_var("DATALIB_DAG_NOW", "2364-04-11T10:00:00+00:00");
    let first = scan_and_commit(&raw_path, &cache).await;

    // Forget which files were read, so the second pass opens every one
    // of them again and rewrites every row from the same bytes.
    let db = RawDb::open(&db_path_for(&raw_path)).await.unwrap();
    sqlx::query("DELETE FROM ingested_files")
        .execute(db.pool())
        .await
        .unwrap();
    datalib_etl::doltlite_raw::commit_run(db.pool(), "forget the cursor")
        .await
        .unwrap();
    let forgotten = datalib_etl::doltlite_raw::head_commit(db.pool())
        .await
        .unwrap()
        .expect("the delete committed");
    db.close().await;

    // A different clock, so a stamp the store mints has somewhere to move.
    std::env::set_var("DATALIB_DAG_NOW", "2364-04-11T10:05:00+00:00");
    let second = scan_and_commit(&raw_path, &cache).await;

    let db = RawDb::open(&db_path_for(&raw_path)).await.unwrap();
    let changed = datalib_etl::doltlite_raw::content_tables_changed(db.pool(), &forgotten, &second)
        .await
        .unwrap();
    let read_again: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM ingested_files")
        .fetch_one(db.pool())
        .await
        .unwrap();
    db.close().await;
    assert_eq!(read_again, 4, "the second pass re-read every rollout");
    assert_eq!(
        changed,
        Vec::<String>::new(),
        "a content table moved when the same bytes were read again"
    );
    assert_ne!(first, second);
}
