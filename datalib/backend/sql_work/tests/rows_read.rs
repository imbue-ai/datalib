//! The rows one piece of work read, counted through sqlx's own threads:
//! what a server's request line says it took to answer.

use datalib_sql::test_sql;
use sqlx::sqlite::SqlitePoolOptions;
use std::sync::Once;
use tracing_subscriber::layer::SubscriberExt;

/// Every span below the counter's is disabled, as a library at
/// `log_level = "error"` has them, so the count must need only its own.
fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let filter = tracing_subscriber::EnvFilter::new("error");
        tracing::subscriber::set_global_default(tracing_subscriber::registry().with(filter))
            .unwrap();
        datalib_sql_work::install();
    });
}

/// A store of `1..=100`.
async fn store(dir: &std::path::Path) -> sqlx::SqlitePool {
    install();
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(datalib_sqlite_open::file(&dir.join("s.sqlite")).create_if_missing(true))
        .await
        .unwrap();
    test_sql!("CREATE TABLE t (k INTEGER PRIMARY KEY)")
        .query()
        .execute(&pool)
        .await
        .unwrap();
    test_sql!(
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 100) \
         INSERT INTO t SELECT x FROM c"
    )
    .query()
    .execute(&pool)
    .await
    .unwrap();
    pool
}

async fn read(pool: &sqlx::SqlitePool, n: i64) -> usize {
    test_sql!("SELECT k FROM t WHERE k <= ?")
        .query()
        .bind(n)
        .fetch_all(pool)
        .await
        .unwrap()
        .len()
}

#[tokio::test]
async fn the_rows_of_every_statement_are_counted_and_no_one_elses() {
    let dir = tempfile::tempdir().unwrap();
    let pool = store(dir.path()).await;
    let ((), rows) = datalib_sql_work::counted(async {
        read(&pool, 30).await;
        read(&pool, 5).await;
        read(&pool, 0).await;
    })
    .await;
    assert_eq!(rows.map(|c| c.rows), Some(35));

    // Two at once share the one connection, and its one thread.
    let (a, b) = futures::join!(
        datalib_sql_work::counted(read(&pool, 7)),
        datalib_sql_work::counted(read(&pool, 11)),
    );
    assert_eq!(
        (a.1.map(|c| c.rows), b.1.map(|c| c.rows)),
        (Some(7), Some(11))
    );

    // Read outside any count.
    read(&pool, 100).await;
    let ((), rows) = datalib_sql_work::counted(async {}).await;
    assert_eq!(rows.map(|c| c.rows), Some(0));
}

/// A count says what was read, not what was asked for: a search whose
/// caller stops at the first row of a hundred read one.
#[tokio::test]
async fn a_statement_stopped_early_counts_what_it_returned() {
    let dir = tempfile::tempdir().unwrap();
    let pool = store(dir.path()).await;
    let (_, rows) = datalib_sql_work::counted(async {
        test_sql!("SELECT k FROM t")
            .query()
            .fetch_optional(&pool)
            .await
    })
    .await;
    assert_eq!(rows.map(|c| c.rows), Some(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn work_on_another_thread_is_counted_when_it_carries_the_count() {
    let dir = tempfile::tempdir().unwrap();
    let pool = store(dir.path()).await;
    let handle = tokio::runtime::Handle::current();
    let (pool2, handle2) = (pool.clone(), handle.clone());
    let (n, rows) = datalib_sql_work::counted(async move {
        tokio::task::spawn_blocking(datalib_sql_work::with_count(move || {
            handle2.block_on(read(&pool2, 40))
        }))
        .await
        .unwrap()
    })
    .await;
    assert_eq!((n, rows.map(|c| c.rows)), (40, Some(40)));

    let (n, rows) = tokio::task::spawn_blocking(move || {
        datalib_sql_work::counted_blocking(|| handle.block_on(read(&pool, 3)))
    })
    .await
    .unwrap();
    assert_eq!((n, rows.map(|c| c.rows)), (3, Some(3)));
}
