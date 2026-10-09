//! A store written while each thread's stamp was a `replies_pages` row
//! opens under this build with a `threads` row held at that stamp,
//! nothing to fetch again, and the old table gone: rung 1 of
//! `schema_raw::LADDER`.

use datalib_etl::doltlite_raw::{self as dr, WirePayloadRow};
use datalib_etl_slack::ingest::schema_raw::{slack_message_key, MessageRow, THREADS};
use datalib_etl_slack::ingest::{db_path_for, RawDb};
use datalib_etl_web::owed;

const STAMPS_DDL: &str = "CREATE TABLE IF NOT EXISTS replies_pages (
    id TEXT PRIMARY KEY, channel_id TEXT NOT NULL, thread_ts TEXT NOT NULL, latest_reply TEXT NULL
)";

/// The thread read whole, and the one only ever tried.
const READ: &str = "1.0";
const TRIED: &str = "5.0";

async fn thread_root(pool: &sqlx::SqlitePool, ts: &str, latest_reply: &str) {
    let payload = format!(
        r#"{{"ts":"{ts}","thread_ts":"{ts}","reply_count":2,"latest_reply":"{latest_reply}"}}"#
    );
    sqlx::query(
        "INSERT INTO messages (id, payload, team_id, channel_id, ts, thread_ts, \
         thread_root_uuid, is_thread_root) VALUES (?, jsonb(?), 'T1', 'C1', ?, ?, ?, 1)",
    )
    .bind(slack_message_key("T1", "C1", ts))
    .bind(payload)
    .bind(ts)
    .bind(ts)
    .bind(slack_message_key("T1", "C1", ts))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO messages_bookkeeping (id, attempt_count) VALUES (?, 1)")
        .bind(slack_message_key("T1", "C1", ts))
        .execute(pool)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_store_with_thread_stamps_opens_holding_each_thread_at_its_stamp() {
    let d = tempfile::tempdir().unwrap();
    let path = db_path_for(d.path());
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let ddl = [
        MessageRow::ddl(),
        dr::bookkeeping_ddl_for("messages"),
        STAMPS_DDL.to_string(),
        dr::bookkeeping_ddl_for("replies_pages"),
    ];
    let ddl: Vec<&str> = ddl.iter().map(String::as_str).collect();
    let pool = dr::open(&path, &ddl).await.unwrap();
    thread_root(&pool, READ, "3.0").await;
    thread_root(&pool, TRIED, "7.0").await;
    // The stamp a whole read wrote, with the sidecar the upsert gave it.
    let mut tx = pool.begin().await.unwrap();
    sqlx::query("INSERT INTO replies_pages VALUES (?, 'C1', ?, '3.0')")
        .bind(slack_message_key("T1", "C1", READ))
        .bind(READ)
        .execute(&mut *tx)
        .await
        .unwrap();
    dr::record_object_attempt(
        &mut tx,
        "replies_pages",
        &slack_message_key("T1", "C1", READ),
        None,
    )
    .await
    .unwrap();
    // The stub a failed read left, with its problem row.
    sqlx::query("INSERT INTO replies_pages VALUES (?, 'C1', ?, NULL)")
        .bind(slack_message_key("T1", "C1", TRIED))
        .bind(TRIED)
        .execute(&mut *tx)
        .await
        .unwrap();
    dr::record_object_error(
        &mut tx,
        "replies_pages",
        &slack_message_key("T1", "C1", TRIED),
        "internal_error",
    )
    .await
    .unwrap();
    tx.commit().await.unwrap();
    dr::commit_run(&pool, "an earlier build").await.unwrap();
    pool.close().await;

    let db = RawDb::open(&path).await.unwrap();
    let held: Vec<(String, bool, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT t.id, b.fetched_at_utc IS NOT NULL, b.held_version, b.last_error \
         FROM threads t JOIN threads_bookkeeping b ON b.id = t.id ORDER BY t.id",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert_eq!(
        held,
        [
            (
                slack_message_key("T1", "C1", READ),
                true,
                Some("3.0".to_string()),
                None
            ),
            (
                slack_message_key("T1", "C1", TRIED),
                false,
                None,
                Some("internal_error".to_string())
            ),
        ]
    );
    let owed: Vec<String> = owed::owed(db.pool(), THREADS, db.threads_listed("C1").await.unwrap())
        .await
        .unwrap()
        .into_iter()
        .map(|l| l.key)
        .collect();
    assert_eq!(
        owed,
        [slack_message_key("T1", "C1", TRIED)],
        "the thread read whole is not fetched again; the one only tried is"
    );
    let problems: Vec<String> = sqlx::query_scalar("SELECT scope_key FROM problems")
        .fetch_all(db.pool())
        .await
        .unwrap();
    assert_eq!(problems, [format!("threads:T1#C1#{TRIED}")]);
    let tables: Vec<String> = sqlx::query_scalar(
        "SELECT name FROM sqlite_master WHERE type = 'table' AND name LIKE 'replies_pages%'",
    )
    .fetch_all(db.pool())
    .await
    .unwrap();
    assert!(tables.is_empty(), "{tables:?}");
    let version: String =
        sqlx::query_scalar("SELECT value FROM _datalib_meta WHERE key = 'schema_version'")
            .fetch_one(db.pool())
            .await
            .unwrap();
    assert_eq!(version, "1");
    db.close().await;
}
