//! The census on a real store: each statement is recorded once however often
//! it runs, a scan of three rows as scanned and a lookup by index as not. One
//! test in this binary, because the census folder is read from the
//! environment once.

use std::path::Path;

use datalib_sql::test_sql;
use sqlx::sqlite::SqlitePoolOptions;

fn recorded(dir: &Path) -> Vec<serde_json::Value> {
    std::fs::read_dir(dir)
        .unwrap()
        .flat_map(|f| {
            std::fs::read_to_string(f.unwrap().path())
                .unwrap()
                .lines()
                .map(|l| serde_json::from_str(l).unwrap())
                .collect::<Vec<_>>()
        })
        .collect()
}

#[tokio::test]
async fn each_statement_is_recorded_once_with_whether_it_scanned() {
    let census = tempfile::tempdir().unwrap();
    // Safety: the one test in this binary, before any thread reads it.
    unsafe { std::env::set_var(datalib_sql_work::CENSUS_ENV, census.path()) };
    datalib_sql_work::install();

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("store.doltlite_db");
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(datalib_sqlite_open::file(&path).create_if_missing(true))
        .await
        .unwrap();
    for sql in [
        test_sql!("CREATE TABLE t (k INTEGER PRIMARY KEY, v TEXT)"),
        test_sql!("INSERT INTO t VALUES (1, 'a'), (2, 'b'), (3, 'c')"),
    ] {
        sql.query().execute(&pool).await.unwrap();
    }
    let scan = test_sql!("SELECT k FROM t WHERE v = ?");
    let lookup = test_sql!("SELECT v FROM t WHERE k = ?");
    for _ in 0..2 {
        let k: i64 = scan
            .query_scalar()
            .bind("c")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(k, 3);
        let v: String = lookup
            .query_scalar()
            .bind(3)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(v, "c");
    }
    pool.close().await;

    let rows = recorded(census.path());
    let of = |sql: &str| -> Vec<&serde_json::Value> {
        rows.iter().filter(|r| r["statement"] == sql).collect()
    };
    let scans = of(scan.as_str());
    assert_eq!(scans.len(), 1, "{rows:#?}");
    assert_eq!(scans[0]["scanned"], true, "{rows:#?}");
    let lookups = of(lookup.as_str());
    assert_eq!(lookups.len(), 1, "{rows:#?}");
    assert_eq!(lookups[0]["scanned"], false, "{rows:#?}");
    let file = scans[0]["file"].as_str().unwrap();
    assert!(file.ends_with("store.doltlite_db"), "{file}");
}
