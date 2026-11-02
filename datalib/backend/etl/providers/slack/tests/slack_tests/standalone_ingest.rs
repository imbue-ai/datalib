//! The `slack-ingest` binary, run as a person runs it, against a tape.

use std::process::Command;

use datalib_etl::doltlite_raw::open_reader;
use datalib_etl_slack::ingest::db_path_for;
use datalib_etl_slack::recorded::History;
use datalib_etl_web::playback::PLAYBACK_ENV;
use serde_json::json;

use crate::support::{msg, record_general, Tree};

/// The binary used to close its store without committing, so a reader
/// found it empty and the next writer's open threw the rows away.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn what_slack_ingest_fetched_is_on_main_for_a_reader() {
    let t = Tree::new();
    record_general(&t.api);
    History::cold("C1")
        .record(
            &t.api,
            json!([
                msg("1735689600.000000", "hello"),
                msg("1735689660.000000", "again")
            ]),
        )
        .unwrap();
    t.serve();

    let bin = std::path::Path::new(
        &std::env::var("SLACK_INGEST_BIN").expect("SLACK_INGEST_BIN is set by the BUILD rule"),
    )
    .canonicalize()
    .expect("slack-ingest is in runfiles");
    let ran = Command::new(bin)
        .env(PLAYBACK_ENV, &t.playback)
        .arg("--out")
        .arg(&t.out)
        .args([
            "--refresh-window-days",
            "0",
            "--members-only",
            "false",
            "--media",
            "false",
        ])
        .output()
        .unwrap();
    assert!(
        ran.status.success(),
        "slack-ingest failed: {}",
        String::from_utf8_lossy(&ran.stderr)
    );

    let reader = open_reader(&db_path_for(&t.out), None)
        .await
        .unwrap()
        .expect("slack-ingest left nothing committed for a reader");
    let messages: i64 = sqlx::query_scalar("SELECT count(*) FROM messages")
        .fetch_one(reader.pool())
        .await
        .unwrap();
    reader.close().await;
    assert_eq!(messages, 2);
}
