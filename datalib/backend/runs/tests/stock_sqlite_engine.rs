//! Does the store still come out as an *ordinary* SQLite file?

use datalib_runs::{open_or_create, runs_path, SCHEMA};

fn magic(path: &std::path::Path) -> Vec<u8> {
    std::fs::read(path).unwrap().into_iter().take(15).collect()
}

/// Create a store, write to it, and read the header back off disk.
#[tokio::test]
async fn the_store_is_a_stock_sqlite_file() {
    let td = tempfile::tempdir().unwrap();
    let db = runs_path(td.path());

    let pool = open_or_create(&db).await.expect("open the store");
    sqlx::raw_sql(SCHEMA).execute(&pool).await.unwrap();
    pool.close().await;

    assert_eq!(
        magic(&db),
        b"SQLite format 3",
        "the store must be stock SQLite, not doltlite's CTLD format"
    );
}

/// The control, so the test above is checking the opt-out rather than
/// restating what would have happened anyway: the same code *without*
/// the parameter must still produce a doltlite file.
#[tokio::test]
async fn without_the_parameter_doltlite_claims_the_file() {
    let td = tempfile::tempdir().unwrap();
    let db = td.path().join("no-opt-out.sqlite");

    let pool = sqlx::SqlitePool::connect_with(
        sqlx::sqlite::SqliteConnectOptions::new()
            .filename(&db)
            .create_if_missing(true),
    )
    .await
    .unwrap();
    sqlx::raw_sql(SCHEMA).execute(&pool).await.unwrap();
    pool.close().await;

    assert_eq!(
        &magic(&db)[..4],
        b"CTLD",
        "doltlite changed its default for a new file; the store's opt-out \
         may no longer be needed"
    );
}

/// The other way this breaks: data roots have spaces and punctuation in
/// them (this repo lives under "Imbue Dropbox"), and the path has to
/// survive being spliced into a `file:` URI.
#[tokio::test]
async fn an_awkward_path_still_opens() {
    let td = tempfile::tempdir().unwrap();
    let db = runs_path(&td.path().join("Imbue Dropbox 100%41 #1"));

    let pool = open_or_create(&db).await.expect("open the store");
    sqlx::raw_sql(SCHEMA).execute(&pool).await.unwrap();
    pool.close().await;

    assert_eq!(magic(&db), b"SQLite format 3");
}
