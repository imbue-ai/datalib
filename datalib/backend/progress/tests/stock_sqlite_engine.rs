//! Does the bus still come out as an *ordinary* SQLite file?
//!
//! Every SQLite handle in this tree is doltlite, whose default for a new
//! file is its own `CTLD` prolly-tree format. The bus opts out with
//! doltlite's `doltlite_engine=sqlite` URI parameter, which has to
//! survive sqlx passing our filename through verbatim. A sqlx upgrade
//! could break that, and the symptom would not be an error — just a bus
//! that works, slowly, in the wrong format. So: assert the first bytes.

use datalib_progress::{open_or_create, progress_path, SCHEMA};

fn magic(path: &std::path::Path) -> Vec<u8> {
    std::fs::read(path).unwrap().into_iter().take(15).collect()
}

/// Create a bus, write to it, and read the header back off disk.
///
/// Kept on an unremarkable path on purpose: if the opt-out breaks, this
/// fails on the magic bytes and says so, rather than on some path
/// mangling that happens to break first.
#[tokio::test]
async fn the_bus_is_a_stock_sqlite_file() {
    let td = tempfile::tempdir().unwrap();
    let db = progress_path(td.path());

    let pool = open_or_create(&db).await.expect("open the bus");
    sqlx::query(SCHEMA).execute(&pool).await.unwrap();
    pool.close().await;

    assert_eq!(
        magic(&db),
        b"SQLite format 3",
        "the bus must be stock SQLite, not doltlite's CTLD format"
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
    sqlx::query(SCHEMA).execute(&pool).await.unwrap();
    pool.close().await;

    assert_eq!(
        &magic(&db)[..4],
        b"CTLD",
        "doltlite changed its default for a new file; the bus's opt-out \
         may no longer be needed"
    );
}

/// The other way this breaks: data roots have spaces and punctuation in
/// them (this repo lives under "Imbue Dropbox"), and the path has to
/// survive being spliced into a `file:` URI.
///
/// `%41` is the case with teeth — SQLite percent-decodes the path
/// portion, so unescaped it becomes a literal `A` and we open a
/// directory that does not exist. A bare `%` followed by a space would
/// pass either way and prove nothing.
#[tokio::test]
async fn an_awkward_path_still_opens() {
    let td = tempfile::tempdir().unwrap();
    let db = progress_path(&td.path().join("Imbue Dropbox 100%41 #1"));

    let pool = open_or_create(&db).await.expect("open the bus");
    sqlx::query(SCHEMA).execute(&pool).await.unwrap();
    pool.close().await;

    assert_eq!(magic(&db), b"SQLite format 3");
}
