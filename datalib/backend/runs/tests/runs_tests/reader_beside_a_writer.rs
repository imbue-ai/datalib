//! A reader of the store does not queue behind its writers.

use std::time::Duration;

use datalib_runs::{
    open_or_create, runs_path, versions, LogRow, Process, ProcessLogWriter, Retention,
};

/// Far under the store's ten-second busy timeout, which is how long a
/// reader waits on a writer before SQLite gives up on it.
const ANSWERED_WITHIN: Duration = Duration::from_secs(3);

/// The Sources card once took fifteen seconds to load mid-sync: every
/// read of the run store waited out the writers' commits, which a busy
/// disk had made seconds long. A read must be answered while a writer
/// holds the store's write lock.
#[tokio::test]
async fn a_read_is_answered_while_a_writer_holds_the_lock() {
    let td = tempfile::tempdir().unwrap();
    let server =
        ProcessLogWriter::start(td.path(), Process::Http, None, Retention::default()).unwrap();
    server.log(LogRow {
        ts_utc: "2026-08-31T10:00:00+01:00".into(),
        level: "info".into(),
        msg: "a line, so the store has a part to report".into(),
        ..Default::default()
    });
    server.flush().await;
    drop(server);

    let writer = open_or_create(&runs_path(td.path())).await.unwrap();
    let mut held = writer.acquire().await.unwrap();
    sqlx::raw_sql("BEGIN EXCLUSIVE; UPDATE store_changes SET version = version + 1")
        .execute(&mut *held)
        .await
        .unwrap();

    let read = tokio::time::timeout(ANSWERED_WITHIN, versions(td.path())).await;

    sqlx::raw_sql("ROLLBACK").execute(&mut *held).await.unwrap();
    drop(held);
    writer.close().await;
    let parts = read
        .expect("the read waited on the writer's lock")
        .expect("the read answered");
    assert!(!parts.is_empty(), "the read came back empty: {parts:?}");
}

/// Bytes 18 and 19 of a SQLite header: 1 for a rollback journal, 2 for WAL.
fn journal_bytes(path: &std::path::Path) -> [u8; 2] {
    let head = std::fs::read(path).unwrap();
    [head[18], head[19]]
}

/// A store the rollback-journal builds left behind turns to WAL at its
/// first open, and keeps the lines it held.
#[tokio::test]
async fn a_store_in_the_old_journal_mode_turns_to_wal_and_keeps_its_lines() {
    let td = tempfile::tempdir().unwrap();
    let path = runs_path(td.path());
    // Stamped now: a server's lines older than its retention are pruned
    // when the next writer starts.
    let line = |msg: &str| LogRow {
        ts_utc: datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339(),
        level: "info".into(),
        msg: msg.into(),
        ..Default::default()
    };
    let before =
        ProcessLogWriter::start(td.path(), Process::Http, None, Retention::default()).unwrap();
    before.log(line("from the old build"));
    before.flush().await;
    drop(before);
    let old = open_or_create(&path).await.unwrap();
    sqlx::raw_sql("PRAGMA journal_mode = DELETE")
        .execute(&old)
        .await
        .unwrap();
    old.close().await;
    assert_eq!(
        journal_bytes(&path),
        [1, 1],
        "the store is set up as the old builds left it"
    );

    let after =
        ProcessLogWriter::start(td.path(), Process::Http, None, Retention::default()).unwrap();
    after.log(line("from this build"));
    after.flush().await;
    drop(after);

    assert_eq!(journal_bytes(&path), [2, 2], "the store is in WAL mode");
    let query = datalib_runs::LogQuery {
        q: "",
        cursor: datalib_runs::LogCursor::After(0),
        limit: 100,
    };
    let msgs: Vec<String> = datalib_runs::log_query(td.path(), &query)
        .await
        .unwrap()
        .into_iter()
        .map(|l| l.msg.clone())
        .collect();
    for msg in ["from the old build", "from this build"] {
        assert!(
            msgs.iter().any(|m| m == msg),
            "{msg:?} is missing from {msgs:?}"
        );
    }
}

/// A store whose recorded shape is not this build's is refused by every
/// reader, naming the store, where each once answered as if it were
/// empty; the next writer's open brings it to this build's shape, and
/// it reads again.
#[tokio::test]
async fn a_store_in_another_shape_is_refused_not_read_as_empty() {
    let td = tempfile::tempdir().unwrap();
    let write_a_line = || {
        let server =
            ProcessLogWriter::start(td.path(), Process::Http, None, Retention::default()).unwrap();
        server.log(LogRow {
            ts_utc: "2026-08-31T10:00:00+01:00".into(),
            level: "info".into(),
            msg: "a line".into(),
            ..Default::default()
        });
        server
    };
    let server = write_a_line();
    server.flush().await;
    drop(server);
    assert!(!versions(td.path()).await.unwrap().is_empty());

    let pool = open_or_create(&runs_path(td.path())).await.unwrap();
    sqlx::query("UPDATE _datalib_meta SET value = 'another' WHERE key = 'schema_hash'")
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
    let refused = versions(td.path()).await.unwrap_err();
    let other = datalib_store_meta::other_shape(&refused).expect("an OtherShape");
    assert_eq!(other.store, runs_path(td.path()));
    assert!(datalib_runs::processes(td.path(), None, None, 10)
        .await
        .is_err());

    let server = write_a_line();
    server.flush().await;
    drop(server);
    assert!(!versions(td.path()).await.unwrap().is_empty());
}
