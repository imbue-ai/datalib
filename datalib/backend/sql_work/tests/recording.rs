//! A recording holds each statement run on a file under its folder, with
//! the work it did, and nothing run on a file elsewhere.

use std::path::Path;

use datalib_sql::test_sql;
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};

async fn store(dir: &Path) -> SqlitePool {
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(
            datalib_sqlite_open::file(&dir.join("store.doltlite_db")).create_if_missing(true),
        )
        .await
        .unwrap();
    for sql in [
        test_sql!("CREATE TABLE t (k INTEGER PRIMARY KEY, v TEXT)"),
        test_sql!(
            "WITH RECURSIVE n(k) AS (SELECT 1 UNION ALL SELECT k + 1 FROM n WHERE k < 100) \
         INSERT INTO t SELECT k, 'v' || k FROM n"
        ),
    ] {
        sql.query().execute(&pool).await.unwrap();
    }
    pool
}

#[tokio::test(flavor = "multi_thread")]
async fn a_recording_holds_its_folders_statements_and_their_work() {
    let (mine, other) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let recording = datalib_sql_work::record_under(mine.path());
    let (pool, elsewhere) = (store(mine.path()).await, store(other.path()).await);
    let scan = test_sql!("SELECT count(*) FROM t WHERE v = ?");
    let lookup = test_sql!("SELECT v FROM t WHERE k = ?");
    for pool in [&pool, &elsewhere] {
        scan.query().bind("v7").fetch_one(pool).await.unwrap();
        lookup.query().bind(7).fetch_one(pool).await.unwrap();
    }
    pool.close().await;
    elsewhere.close().await;

    let ran = recording.take();
    let of = |sql: &str| {
        let runs: Vec<_> = ran.iter().filter(|r| r.sql == sql).collect();
        assert_eq!(runs.len(), 1, "{sql}: {ran:#?}");
        runs[0].clone()
    };
    let (scan, lookup) = (of(scan.as_str()), of(lookup.as_str()));
    assert!(scan.file.ends_with("store.doltlite_db"), "{}", scan.file);
    assert!(scan.work.fullscan_steps >= 90, "{scan:?}");
    assert_eq!(lookup.work.fullscan_steps, 0, "{lookup:?}");
    assert!(
        lookup.work.vm_steps < scan.work.vm_steps / 10,
        "{lookup:?} {scan:?}"
    );
    assert!(recording.take().is_empty(), "a take empties it");
}
