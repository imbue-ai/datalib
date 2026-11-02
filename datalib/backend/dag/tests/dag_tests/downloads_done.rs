//! An app that reads only the raw stores waits for its downloads, not for
//! the index steps after them, by asking the loop's record whether each
//! download started after its request opened and has finished since
//! (`docs/user/building_on_datalib.md`, "To wait for the downloads only").
//! That compares a step's stamps with the request's, so both must keep
//! the same precision.

use std::path::Path;
use std::process::Command;

fn dag_bin() -> std::path::PathBuf {
    std::path::absolute(std::env::var("DATALIB_DAG_BIN").expect("DATALIB_DAG_BIN is set"))
        .expect("absolute DATALIB_DAG_BIN")
}

/// The query the guide gives, for one request and one step.
const DONE: &str = "
  SELECT s.step, s.last_status
    FROM steps s, requests r
   WHERE r.id = ? AND s.step = ?
     AND s.last_started_at_utc > r.opened_at_utc
     AND s.last_finished_at_utc >= s.last_started_at_utc";

/// The step's stamps were cut to the second while the request's kept its
/// microseconds, so a download that started in the second its request
/// opened read as having started before it, and never as done.
#[tokio::test]
async fn a_download_reads_as_done_for_the_request_that_ran_it() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("config.toml"),
        "[[steps]]\nid = \"a/src\"\ncommand = \"sh -c 'mkdir -p a/src && date > a/src/x'\"\n",
    )
    .unwrap();
    let out = Command::new(dag_bin())
        .arg(root.join("config.toml"))
        .args(["--sync", "a/src"])
        .output()
        .expect("run datalib-dag");
    assert!(out.status.success(), "{out:?}");

    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect(&format!(
            "sqlite://{}?mode=ro",
            Path::new(root).join("system/supervisor.sqlite").display()
        ))
        .await
        .unwrap();
    let request: String = sqlx::query_scalar("SELECT id FROM requests")
        .fetch_one(&pool)
        .await
        .unwrap();
    let done: Option<(String, String)> = sqlx::query_as(DONE)
        .bind(&request)
        .bind("a/src")
        .fetch_optional(&pool)
        .await
        .unwrap();
    let stamps: Vec<(String, String)> =
        sqlx::query_as("SELECT r.opened_at_utc, s.last_started_at_utc FROM requests r, steps s")
            .fetch_all(&pool)
            .await
            .unwrap();
    pool.close().await;
    assert_eq!(
        done,
        Some(("a/src".into(), "succeeded".into())),
        "request opened, step started: {stamps:?}"
    );
}
