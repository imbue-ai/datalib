//! A built-in step's line carries its own stamp, and the run store files
//! the line under that stamp's offset. It must be the offset the runner's
//! clock is in, as every other line beside it is.

use std::path::{Path, PathBuf};
use std::process::Command;

fn bin(var: &str) -> PathBuf {
    std::path::absolute(std::env::var(var).unwrap_or_else(|_| panic!("{var} is set")))
        .unwrap_or_else(|e| panic!("{var}: {e}"))
}

/// A zone three hours ahead of UTC, written as a POSIX rule so it needs no
/// zoneinfo file on the host.
const ZONE: &str = "DLT-3";

/// The step logged through tracing-subscriber's own timer, which writes
/// UTC; its lines were filed as `+00:00` while the runner's, at the same
/// moment, carried the machine's real offset.
#[tokio::test]
async fn a_step_line_is_filed_under_the_runner_s_offset() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    std::fs::write(
        root.join("config.toml"),
        format!(
            "[[steps]]\nid = \"probe/step\"\n\
             command = \"sh -c 'mkdir -p probe/step && exec \\\"{}\\\"'\"\n",
            bin("LOG_PROBE").display()
        ),
    )
    .unwrap();
    // The runner's own environment, as a launch gives it one: the step is
    // handed `TZ` through the runner's allow list, not by this test.
    let out = Command::new(bin("DATALIB_DAG_BIN"))
        .arg(root.join("config.toml"))
        .args(["--sync", "probe/step"])
        .env("TZ", ZONE)
        .output()
        .expect("run datalib-dag");
    assert!(out.status.success(), "{out:?}");

    let pool = sqlx::sqlite::SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect(&format!(
            "sqlite://{}?mode=ro",
            datalib_runtime::layout::runs_db(Path::new(root)).display()
        ))
        .await
        .unwrap();
    let runner: Vec<Option<String>> =
        sqlx::query_scalar("SELECT tz_offset FROM processes WHERE process = 'dag'")
            .fetch_all(&pool)
            .await
            .unwrap();
    let step: Vec<(String, Option<String>)> = sqlx::query_as(
        "SELECT l.ts_utc, l.tz_offset FROM log l JOIN processes p USING (process_id) \
         WHERE p.process = 'step' AND l.msg = 'a line that stamps itself'",
    )
    .fetch_all(&pool)
    .await
    .unwrap();
    pool.close().await;

    assert_eq!(
        runner,
        vec![Some("+03:00".to_string())],
        "the runner's zone"
    );
    assert_eq!(step.len(), 1, "the step's line: {step:?}\n{out:?}");
    assert_eq!(step[0].1.as_deref(), Some("+03:00"), "{step:?}");
}
