//! The counter on a real store, through sqlx, as every binary runs it: a
//! lookup that reads the whole table for one row is logged with its file
//! and its parameters' shapes, and the same lookup by an index is not.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

use sqlx::sqlite::SqlitePoolOptions;
use tracing::field::{Field, Visit};
use tracing_subscriber::layer::{Context, SubscriberExt};
use tracing_subscriber::Layer;

type Line = HashMap<String, String>;

/// Every heavy-statement line this process wrote. The counter writes from
/// sqlx's worker thread, so the subscriber is the process's.
fn lines() -> &'static Mutex<Vec<Line>> {
    static LINES: OnceLock<Mutex<Vec<Line>>> = OnceLock::new();
    LINES.get_or_init(|| {
        let subscriber = tracing_subscriber::registry().with(Collect);
        tracing::subscriber::set_global_default(subscriber).unwrap();
        Mutex::new(Vec::new())
    })
}

struct Collect;

impl<S: tracing::Subscriber> Layer<S> for Collect {
    fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
        if event.metadata().target() != datalib_sql_work::TARGET {
            return;
        }
        let mut line = Line::new();
        event.record(&mut Fields(&mut line));
        lines().lock().unwrap().push(line);
    }
}

struct Fields<'a>(&'a mut Line);

impl Visit for Fields<'_> {
    fn record_str(&mut self, field: &Field, value: &str) {
        self.0.insert(field.name().into(), value.into());
    }
    fn record_u64(&mut self, field: &Field, value: u64) {
        self.0.insert(field.name().into(), value.to_string());
    }
    fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
        self.0.insert(field.name().into(), format!("{value:?}"));
    }
}

/// The lines about `file`, named by its folder and its own name: SQLite
/// reports the path it resolved, which on a mac is under `/private`.
fn lines_about(file: &Path) -> Vec<Line> {
    let folder = file
        .parent()
        .unwrap()
        .file_name()
        .unwrap()
        .to_string_lossy();
    let tail = format!("{folder}/{}", file.file_name().unwrap().to_string_lossy());
    lines()
        .lock()
        .unwrap()
        .iter()
        .filter(|l| l.get("file").is_some_and(|f| f.ends_with(&tail)))
        .cloned()
        .collect()
}

const ROWS: u64 = 300_000;

/// A doltlite store of `ROWS` rows, `k` unique and `flag` one of two.
async fn store(dir: &Path) -> (std::path::PathBuf, sqlx::SqlitePool) {
    lines();
    datalib_sql_work::install();
    let path = dir.join("store.doltlite_db");
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(datalib_sqlite_open::file(&path).create_if_missing(true))
        .await
        .unwrap();
    sqlx::query("CREATE TABLE t (k INTEGER NOT NULL, flag INTEGER NOT NULL, v TEXT)")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < ?) \
         INSERT INTO t SELECT x, x % 2, 'v' || x FROM c",
    )
    .bind(ROWS as i64)
    .execute(&pool)
    .await
    .unwrap();
    (path, pool)
}

const LOOKUP: &str = "SELECT v FROM t WHERE k = ?";

#[tokio::test]
async fn a_lookup_that_reads_the_table_is_logged_and_one_by_index_is_not() {
    let dir = tempfile::tempdir().unwrap();
    let (path, pool) = store(dir.path()).await;
    assert!(
        lines_about(&path).is_empty(),
        "the fill answered with every row it wrote"
    );

    // The last key, so the scan reads every row before it finds it.
    let v: String = sqlx::query_scalar(LOOKUP)
        .bind(ROWS as i64)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(v, format!("v{ROWS}"));
    let logged = lines_about(&path);
    assert_eq!(logged.len(), 1, "{logged:#?}");
    let line = &logged[0];
    assert_eq!(line["statement"], LOOKUP);
    assert_eq!(line["params"], "int", "the value's shape, not the value");
    assert_eq!(line["rows"], "1");
    assert!(
        line["fullscan_steps"].parse::<u64>().unwrap() >= ROWS - 1,
        "{line:#?}"
    );

    sqlx::query("CREATE INDEX t_by_k ON t (k)")
        .execute(&pool)
        .await
        .unwrap();
    let v: String = sqlx::query_scalar(LOOKUP)
        .bind(ROWS as i64 - 2)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(v, format!("v{}", ROWS - 2));
    assert_eq!(lines_about(&path).len(), 1, "a lookup by index is cheap");
    pool.close().await;
}

/// Guards the Library card's node query (#1926): it searched an index of a
/// two-valued column and so read half the table, scanning none of it.
#[tokio::test]
async fn a_search_by_an_unselective_index_is_logged_though_it_scans_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let (path, pool) = store(dir.path()).await;
    sqlx::query("CREATE INDEX t_by_flag ON t (flag)")
        .execute(&pool)
        .await
        .unwrap();

    let v: String =
        sqlx::query_scalar("SELECT v FROM t INDEXED BY t_by_flag WHERE flag = 1 AND k = ?")
            .bind(ROWS as i64 - 1)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(v, format!("v{}", ROWS - 1));
    let logged = lines_about(&path);
    assert_eq!(logged.len(), 1, "{logged:#?}");
    assert_eq!(logged[0]["fullscan_steps"], "0", "{logged:#?}");
    pool.close().await;
}
