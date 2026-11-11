//! `/sql` over stores made here: a plain SQLite file and a doltlite store
//! with two commits.

use std::path::Path;
use std::time::Duration;

use axum::http::StatusCode;
use serde_json::{json, Value};
use sqlx::sqlite::SqliteConnection;
use sqlx::{Connection, Executor};
use tokio::sync::Semaphore;

use super::*;

const PLAIN: &str = "g/s/plain.sqlite";
const DOLT: &str = "g/s/entities.doltlite_db";

async fn plain_store(root: &Path) {
    let path = root.join(PLAIN);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let opts = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(datalib_runtime::plain_sqlite::uri(&path))
        .create_if_missing(true);
    let mut conn = SqliteConnection::connect_with(&opts).await.unwrap();
    conn.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT, score REAL, data BLOB, big INTEGER);
         INSERT INTO t VALUES (1, 'riker', 1.5, x'0001ff', 9007199254740993),
                              (2, 'troi', NULL, NULL, NULL),
                              (3, 'data', 3.0, NULL, NULL),
                              (4, 'worf', 4.0, NULL, NULL),
                              (5, 'crusher', 5.0, NULL, NULL);",
    )
    .await
    .unwrap();
    conn.close().await.unwrap();
}

/// A doltlite store whose `main` holds `crew` with `rows` rows after each
/// call, committed; returns the commit.
async fn dolt_commit_rows(root: &Path, names: &[&str]) -> String {
    let path = root.join(DOLT);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let opts = datalib_sqlite_open::file(&path).create_if_missing(true);
    let mut conn = SqliteConnection::connect_with(&opts).await.unwrap();
    conn.execute("CREATE TABLE IF NOT EXISTS crew (name TEXT PRIMARY KEY)")
        .await
        .unwrap();
    for name in names {
        sqlx::query("INSERT INTO crew VALUES (?)")
            .bind(*name)
            .execute(&mut conn)
            .await
            .unwrap();
    }
    sqlx::query("SELECT dolt_commit('-Am', 'crew')")
        .execute(&mut conn)
        .await
        .unwrap();
    let head: String = sqlx::query_scalar("SELECT dolt_hashof('HEAD')")
        .fetch_one(&mut conn)
        .await
        .unwrap();
    conn.close().await.unwrap();
    head
}

fn request(store: &str, sql: &str) -> SqlRequest {
    serde_json::from_value(json!({ "store": store, "sql": sql })).unwrap()
}

async fn ask(root: &Path, body: Value) -> Result<Page, Failure> {
    let slots = Semaphore::new(AT_ONCE);
    run(
        root,
        &slots,
        serde_json::from_value(body).unwrap(),
        DEADLINE,
    )
    .await
}

fn file_bytes(root: &Path, store: &str) -> Vec<u8> {
    std::fs::read(root.join(store)).unwrap()
}

/// Paging walks the whole result once: each page names where the next
/// starts, and the last names none.
#[tokio::test]
async fn pages_walk_the_whole_result_once() {
    let root = tempfile::tempdir().unwrap();
    plain_store(root.path()).await;
    let mut names = Vec::new();
    let mut offset = Some(0);
    let mut pages = 0;
    while let Some(at) = offset {
        let page = ask(
            root.path(),
            json!({"store": PLAIN, "sql": "SELECT name FROM t ORDER BY id;", "offset": at, "limit": 2}),
        )
        .await
        .unwrap();
        assert_eq!(page.columns, ["name"]);
        names.extend(page.rows.into_iter().map(|r| r[0].clone()));
        offset = page.next_offset;
        pages += 1;
    }
    assert_eq!(names, ["riker", "troi", "data", "worf", "crusher"]);
    assert_eq!(pages, 3);
}

#[tokio::test]
async fn values_keep_their_sqlite_type() {
    let root = tempfile::tempdir().unwrap();
    plain_store(root.path()).await;
    let page = ask(
        root.path(),
        json!({"store": PLAIN, "sql": "SELECT id, name, score, data, big FROM t WHERE id = 1"}),
    )
    .await
    .unwrap();
    assert_eq!(page.columns, ["id", "name", "score", "data", "big"]);
    assert_eq!(page.kind, StoreKind::Sqlite);
    assert_eq!(page.commit, None);
    assert_eq!(
        serde_json::to_string(&page.rows[0]).unwrap(),
        r#"[1,"riker",1.5,{"base64":"AAH/"},9007199254740993]"#
    );
}

/// The same rows, by every way a parameter is written: `?`, `?NNN`, and
/// `:x`, `@x` or `$x` keyed with or without the sigil, one name used twice,
/// and a blob sent as base64.
#[tokio::test]
async fn params_bind_by_position_or_by_name() {
    let root = tempfile::tempdir().unwrap();
    plain_store(root.path()).await;
    for (sql, params) in [
        (
            "SELECT name FROM t WHERE id > ? AND score < ?",
            json!([2, 5]),
        ),
        (
            "SELECT name FROM t WHERE id > ?2 AND score < ?1",
            json!([5, 2]),
        ),
        (
            "SELECT name FROM t WHERE id IN (SELECT value FROM json_each(?1)) AND ?2",
            json!(["[3, 4]", true]),
        ),
        (
            "SELECT name FROM t WHERE id > :lo AND score < @hi",
            json!({"lo": 2, "@hi": 5}),
        ),
        (
            "SELECT name FROM t WHERE id > $n AND id < $n + 3",
            json!({"n": 2}),
        ),
        (
            "SELECT name FROM t WHERE id IN (:a, :b) AND name != :blob ORDER BY id",
            json!({"a": 3, "b": 4, "blob": {"base64": "AAH/"}}),
        ),
    ] {
        let page = ask(
            root.path(),
            json!({"store": PLAIN, "sql": sql, "params": params}),
        )
        .await
        .unwrap_or_else(|f| panic!("{sql}: {}", f.error));
        assert_eq!(page.rows, [[json!("data")], [json!("worf")]], "{sql}");
    }

    let page = ask(
        root.path(),
        json!({"store": PLAIN, "sql": "SELECT name FROM t WHERE data = :d", "params": {"d": {"base64": "AAH/"}}}),
    )
    .await
    .unwrap();
    assert_eq!(page.rows, [[json!("riker")]]);

    for (sql, params, says) in [
        (
            "SELECT name FROM t WHERE id BETWEEN ? AND ?",
            json!([1]),
            "params holds 1",
        ),
        (
            "SELECT name FROM t WHERE id = ?",
            json!([[2]]),
            "a parameter is a string",
        ),
        (
            "SELECT name FROM t WHERE id = :id",
            json!([2]),
            ":id is a named parameter: send params as an object",
        ),
        (
            "SELECT name FROM t WHERE id = :id",
            json!({"idd": 2}),
            "params has no value for :id",
        ),
        (
            "SELECT name FROM t WHERE id = :id",
            json!({"id": 2, "name": "riker"}),
            "the statement has no parameter name",
        ),
        (
            "SELECT name FROM t WHERE id = ?",
            json!({"id": 2}),
            "takes its value by position",
        ),
        (
            "SELECT name FROM t WHERE id = :id",
            json!({"id": {"base64": "not base64!"}}),
            "standard base64",
        ),
        (
            "SELECT name FROM t WHERE id = :id",
            json!("2"),
            "params is a list",
        ),
        (
            "SELECT name FROM t WHERE id = ?",
            json!([18446744073709551615u64]),
            "largest integer",
        ),
    ] {
        let failure = ask(
            root.path(),
            json!({"store": PLAIN, "sql": sql, "params": params}),
        )
        .await
        .unwrap_err();
        assert_eq!(failure.status, StatusCode::BAD_REQUEST);
        assert!(
            failure.error.contains(says),
            "{says:?} not in {:?}",
            failure.error
        );
    }
}

/// A doltlite page reads one commit: the next page, sent with the commit
/// the first answered, reads that commit though the writer has moved on.
#[tokio::test]
async fn a_doltlite_page_reads_the_commit_it_names() {
    let root = tempfile::tempdir().unwrap();
    let first = dolt_commit_rows(root.path(), &["picard", "riker", "troi"]).await;
    let sql = "SELECT name FROM crew ORDER BY name";
    let page = ask(root.path(), json!({"store": DOLT, "sql": sql, "limit": 2}))
        .await
        .unwrap();
    assert_eq!(page.kind, StoreKind::Doltlite);
    assert_eq!(page.commit.as_deref(), Some(first.as_str()));
    assert_eq!(page.rows, [[json!("picard")], [json!("riker")]]);

    let second = dolt_commit_rows(root.path(), &["data", "worf"]).await;
    assert_ne!(first, second);
    let rest = ask(
        root.path(),
        json!({"store": DOLT, "sql": sql, "limit": 2, "offset": 2, "commit": first}),
    )
    .await
    .unwrap();
    assert_eq!(rest.rows, [[json!("troi")]]);
    assert_eq!(rest.next_offset, None);

    let newest = ask(
        root.path(),
        json!({"store": DOLT, "sql": "SELECT count(*) FROM crew"}),
    )
    .await
    .unwrap();
    assert_eq!(newest.commit.as_deref(), Some(second.as_str()));
    assert_eq!(newest.rows, [[json!(5)]]);
}

/// The reads doltlite offers work through the guard: its hash functions,
/// its log and its diffs, and SQLite's pragma tables.
#[tokio::test]
async fn doltlites_reads_and_the_pragma_tables_pass_the_guard() {
    let root = tempfile::tempdir().unwrap();
    let first = dolt_commit_rows(root.path(), &["picard"]).await;
    let second = dolt_commit_rows(root.path(), &["riker"]).await;
    for (sql, want) in [
        ("SELECT dolt_hashof('HEAD')".to_string(), json!(second)),
        (
            "SELECT count(*) FROM dolt_log() WHERE commit_hash IN (SELECT dolt_hashof('HEAD'))"
                .to_string(),
            json!(1),
        ),
        (
            format!("SELECT to_name FROM dolt_diff_crew('{first}', '{second}')"),
            json!("riker"),
        ),
        (
            "SELECT name FROM pragma_table_info('crew')".to_string(),
            json!("name"),
        ),
        (
            "SELECT commit_hash FROM dolt_history_crew WHERE name = 'riker'".to_string(),
            json!(second),
        ),
        ("VALUES (7)".to_string(), json!(7)),
        (
            "SELECT name FROM sqlite_schema WHERE type = 'table' AND name = 'crew'".to_string(),
            json!("crew"),
        ),
    ] {
        let page = ask(root.path(), json!({"store": DOLT, "sql": sql}))
            .await
            .unwrap_or_else(|f| panic!("{sql}: {}", f.error));
        assert_eq!(page.rows[0][0], want, "{sql}");
    }
    let page = ask(
        root.path(),
        json!({"store": DOLT, "sql": "SELECT name, type FROM pragma_table_info(:table)", "params": {"table": "crew"}}),
    )
    .await
    .unwrap();
    assert_eq!(page.rows, [[json!("name"), json!("TEXT")]]);
}

/// Nothing a caller sends writes: a statement that asks to is refused
/// before it runs, and the files come out byte for byte as they went in.
#[tokio::test]
async fn nothing_a_caller_sends_writes() {
    let root = tempfile::tempdir().unwrap();
    plain_store(root.path()).await;
    dolt_commit_rows(root.path(), &["picard"]).await;
    let before = (
        file_bytes(root.path(), PLAIN),
        file_bytes(root.path(), DOLT),
    );
    let outside = root.path().join("written.db");

    for (store, sql, status, says) in [
        (
            DOLT,
            "SELECT dolt_creds_new()".to_string(),
            StatusCode::FORBIDDEN,
            "dolt_creds_new",
        ),
        (
            DOLT,
            "SELECT dolt_commit('--allow-empty', '-m', 'x')".to_string(),
            StatusCode::FORBIDDEN,
            "dolt_commit",
        ),
        (
            DOLT,
            "SELECT dolt_gc()".to_string(),
            StatusCode::FORBIDDEN,
            "dolt_gc",
        ),
        (
            DOLT,
            "SELECT dolt_push('origin', 'main')".to_string(),
            StatusCode::FORBIDDEN,
            "dolt_push",
        ),
        (
            PLAIN,
            "SELECT 1; SELECT 2".to_string(),
            StatusCode::BAD_REQUEST,
            "syntax error",
        ),
        (
            PLAIN,
            "SELECT 1) LIMIT 1; DELETE FROM t; SELECT (1".to_string(),
            StatusCode::BAD_REQUEST,
            "one SELECT",
        ),
        (
            PLAIN,
            "DELETE FROM t".to_string(),
            StatusCode::BAD_REQUEST,
            "syntax error",
        ),
        (
            PLAIN,
            format!("VACUUM INTO '{}'", outside.display()),
            StatusCode::BAD_REQUEST,
            "syntax error",
        ),
        (
            PLAIN,
            "SELECT * FROM fsdir('.')".to_string(),
            StatusCode::BAD_REQUEST,
            "no such table",
        ),
        (
            PLAIN,
            "SELECT writefile('x', 'y')".to_string(),
            StatusCode::BAD_REQUEST,
            "no such function",
        ),
        (
            PLAIN,
            "SELECT load_extension('x')".to_string(),
            StatusCode::BAD_REQUEST,
            "not authorized",
        ),
    ] {
        let failure = ask(root.path(), json!({"store": store, "sql": sql}))
            .await
            .map(|p| p.rows)
            .expect_err(&sql);
        assert_eq!(failure.status, status, "{sql}: {}", failure.error);
        assert!(
            failure.error.contains(says),
            "{sql}: {says:?} not in {:?}",
            failure.error
        );
    }
    assert!(!outside.exists());
    assert!(!root.path().join("x").exists());
    assert_eq!(
        (
            file_bytes(root.path(), PLAIN),
            file_bytes(root.path(), DOLT)
        ),
        before
    );
}

/// The guard on its own, with no paging around the statement: ATTACH, a
/// write and a refused function are each turned away at prepare.
#[tokio::test]
async fn a_guarded_connection_refuses_attach_and_writes_at_prepare() {
    let root = tempfile::tempdir().unwrap();
    plain_store(root.path()).await;
    let name = datalib_sqlite_open::literal(&root.path().join(PLAIN));
    let mut reader = guard::Reader::open(&name, std::time::Instant::now() + DEADLINE).unwrap();
    let other = root.path().join("other.sqlite");
    for (sql, refused) in [
        (format!("ATTACH '{}' AS o", other.display()), "ATTACH"),
        ("DELETE FROM t".to_string(), "a write"),
        (
            "SELECT dolt_creds_new()".to_string(),
            "the function dolt_creds_new()",
        ),
        (
            "PRAGMA writable_schema = 1".to_string(),
            "PRAGMA writable_schema",
        ),
    ] {
        assert_eq!(
            reader.check(&sql),
            Err(guard::Refusal::Refused(refused.to_string())),
            "{sql}"
        );
    }
}

/// A query still running at its deadline is stopped, and says so.
#[tokio::test]
async fn a_query_past_its_deadline_is_stopped() {
    let root = tempfile::tempdir().unwrap();
    plain_store(root.path()).await;
    let slots = Semaphore::new(AT_ONCE);
    let endless = "WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM n) \
                   SELECT count(*) FROM n";
    let started = std::time::Instant::now();
    let failure = run(
        root.path(),
        &slots,
        request(PLAIN, endless),
        Duration::from_millis(300),
    )
    .await
    .unwrap_err();
    assert_eq!(
        failure.status,
        StatusCode::GATEWAY_TIMEOUT,
        "{}",
        failure.error
    );
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "stopped only after {:?}",
        started.elapsed()
    );
}

#[tokio::test]
async fn a_store_is_named_under_the_root_and_must_be_there() {
    let root = tempfile::tempdir().unwrap();
    plain_store(root.path()).await;
    for (store, status) in [
        ("system/api-token", StatusCode::BAD_REQUEST),
        ("../elsewhere.sqlite", StatusCode::BAD_REQUEST),
        ("g/s/absent.sqlite", StatusCode::NOT_FOUND),
    ] {
        let failure = ask(root.path(), json!({"store": store, "sql": "SELECT 1"}))
            .await
            .unwrap_err();
        assert_eq!(failure.status, status, "{store}: {}", failure.error);
    }
    let failure = ask(
        root.path(),
        json!({"store": PLAIN, "sql": "SELECT 1", "commit": "0".repeat(40)}),
    )
    .await
    .unwrap_err();
    assert!(failure.error.contains("no commits"), "{}", failure.error);

    dolt_commit_rows(root.path(), &["picard"]).await;
    let failure = ask(
        root.path(),
        json!({"store": DOLT, "sql": "SELECT 1", "commit": "0".repeat(40)}),
    )
    .await
    .unwrap_err();
    assert_eq!(failure.status, StatusCode::NOT_FOUND, "{}", failure.error);
}
