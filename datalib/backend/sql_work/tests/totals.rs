//! What a process's statements did, by the name each carries: the rows
//! each loop visited and the index it read, on a real doltlite store, and
//! what one request's statements did.

use std::path::Path;
use std::sync::Once;

use datalib_sql::{sql, test_sql};
use sqlx::sqlite::{SqlitePool, SqlitePoolOptions};
use tracing_subscriber::layer::SubscriberExt;

/// A count needs a subscriber that keeps spans, as a server's has.
fn install() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        tracing::subscriber::set_global_default(
            tracing_subscriber::registry().with(tracing_subscriber::EnvFilter::new("error")),
        )
        .unwrap();
        datalib_sql_work::install();
    });
}

/// `t` holding `1..=1000`, `v` the key modulo 10, with an index on `v`.
async fn store(dir: &Path) -> SqlitePool {
    install();
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
        test_sql!("CREATE TABLE t (k INTEGER PRIMARY KEY, v INTEGER)"),
        test_sql!("CREATE INDEX t_by_v ON t (v)"),
        test_sql!(
            "WITH RECURSIVE n(k) AS (SELECT 1 UNION ALL SELECT k + 1 FROM n WHERE k < 1000) \
             INSERT INTO t SELECT k, k % 10 FROM n"
        ),
    ] {
        sql.query().execute(&pool).await.unwrap();
    }
    pool
}

/// Guards the metric a slow statement is found by: a lookup by an index
/// visits the rows it keeps and says which index, and a scan visits every
/// row; each is counted under its own name, run by run.
#[tokio::test]
async fn each_statements_loops_say_what_they_read_and_how_much() {
    let dir = tempfile::tempdir().unwrap();
    let pool = store(dir.path()).await;
    let by_index = sql!(
        "sql_work.tests.count_by_indexed_value",
        "SELECT count(*) FROM t WHERE v = ?"
    );
    let by_scan = sql!(
        "sql_work.tests.count_by_unindexed_sum",
        "SELECT count(*) FROM t WHERE k + v = ?"
    );
    for _ in 0..2 {
        let n: i64 = by_index
            .query_scalar()
            .bind(3)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(n, 100);
        by_scan.query().bind(7).fetch_one(&pool).await.unwrap();
    }
    pool.close().await;

    let totals = datalib_sql_work::totals();
    let indexed = &totals["sql_work.tests.count_by_indexed_value"];
    assert_eq!(indexed.runs, 2, "{indexed:?}");
    assert_eq!(indexed.rows, 2, "one count a run: {indexed:?}");
    assert_eq!(
        indexed.rows_scanned, 200,
        "the rows with v = 3, each run: {indexed:?}"
    );
    let (lp, visited) = indexed.loops.iter().next().expect("one loop");
    assert!(lp.contains("USING COVERING INDEX t_by_v"), "{lp}");
    assert_eq!(*visited, 200);

    let scanned = &totals["sql_work.tests.count_by_unindexed_sum"];
    assert_eq!(
        scanned.rows_scanned, 2_000,
        "every row, each run: {scanned:?}"
    );
    assert!(
        scanned.loops.keys().all(|l| l.starts_with("SCAN t")),
        "{scanned:?}"
    );
}

/// A request's statements are its own: what a server writes on the
/// request's line, and an applet sends the gateway.
#[tokio::test]
async fn a_count_holds_its_own_statements_by_name() {
    let dir = tempfile::tempdir().unwrap();
    let pool = store(dir.path()).await;
    let lookup = sql!("sql_work.tests.find_by_key", "SELECT v FROM t WHERE k = ?");
    lookup.query().bind(1).fetch_one(&pool).await.unwrap();
    let ((), counted) = datalib_sql_work::counted(async {
        for k in [5, 6, 7] {
            lookup.query().bind(k).fetch_one(&pool).await.unwrap();
        }
    })
    .await;
    pool.close().await;
    let counted = counted.expect("this process counts");
    assert_eq!(counted.rows, 3);
    let work = &counted.statements["sql_work.tests.find_by_key"];
    assert_eq!((work.runs, work.rows_scanned), (3, 3), "{work:?}");
    assert_eq!(counted.statements.len(), 1, "{:?}", counted.statements);

    let header: datalib_sql_work::ByTag =
        serde_json::from_str(&datalib_sql_work::header_value(&counted.statements)).unwrap();
    assert_eq!(header, counted.statements, "the header carries it whole");
}

/// Guards a statement's row count on a store read at one commit: the
/// reader's open runs doltlite's own statements, whose rows are theirs.
#[tokio::test]
async fn a_pinned_readers_count_is_its_own_rows() {
    let dir = tempfile::tempdir().unwrap();
    let pool = store(dir.path()).await;
    test_sql!("SELECT dolt_commit('-Am', 'c')")
        .query()
        .execute(&pool)
        .await
        .unwrap();
    let head = datalib_pin::head(&pool).await.unwrap().unwrap();
    pool.close().await;
    let reader = datalib_pin::open_at(&dir.path().join("store.doltlite_db"), &head)
        .await
        .unwrap();
    let before = datalib_sql_work::totals();
    for _ in 0..8 {
        assert!(datalib_pin::holds_a_table(&reader).await.unwrap());
    }
    reader.close().await;
    let ran = datalib_sql_work::since(&datalib_sql_work::totals(), &before);
    let held = &ran["pin.has_user_table"];
    assert_eq!((held.runs, held.rows), (8, 8), "{ran:#?}");
}
