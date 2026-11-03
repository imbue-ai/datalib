//! An author a stopped run named and did not look up is looked up first
//! by the next run, before its walk. The lookups ran only after the walk,
//! and a stopped run skips them, so a run of stopped syncs left their
//! messages with no one to show as their author.

use std::sync::Arc;

use datalib_etl::control::DownloadControl;
use datalib_etl::progress::{Progress, ProgressSink};
use datalib_etl::stop::StopFlag;
use datalib_etl_slack::ingest::{db_path_for, FetchOptions, RawDb};
use datalib_etl_slack::recorded::{record_user_info, record_workspace};
use serde_json::json;

use crate::support::{fetch_into, Tree};

/// Stops the download at its first progress tick, part-way into its walk.
struct StopAtFirstTick(StopFlag);

impl ProgressSink for StopAtFirstTick {
    fn inc(&self, _delta: u64) {
        self.0.request();
    }
}

async fn stopped_sync(t: &Tree) {
    let stop = StopFlag::new();
    fetch_into(&t.playback, &t.out, |o| FetchOptions {
        progress: Progress::new(Arc::new(StopAtFirstTick(stop.clone()))),
        control: DownloadControl {
            stop: stop.clone(),
            ..Default::default()
        },
        ..o
    })
    .await
    .expect("a stopped run is a shorter run, not a failed one");
    assert!(stop.requested());
}

async fn authors_owed(t: &Tree) -> Vec<String> {
    let db = RawDb::open(&db_path_for(&t.out)).await.unwrap();
    let mut conn = db.pool().acquire().await.unwrap();
    let owed = db.users_owed(&mut conn).await.unwrap();
    drop(conn);
    db.close().await;
    owed.into_iter().map(|l| l.key).collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_runs_authors_are_looked_up_before_the_next_walk() {
    let t = Tree::new();
    // Worf posts in every channel and no `users.list` names him.
    record_workspace(&t.api, &["C1", "C2", "C3"], |i, c| {
        json!([{"ts": format!("1735689600.0001{i:02}"), "user": "U9", "text": format!("in {c}")}])
    })
    .unwrap();
    record_user_info(&t.api, json!({"id": "U9", "name": "worf"})).unwrap();
    t.serve();

    stopped_sync(&t).await;
    assert_eq!(
        authors_owed(&t).await,
        ["U9"],
        "the stopped run named Worf and did not look him up"
    );

    stopped_sync(&t).await;
    assert!(
        authors_owed(&t).await.is_empty(),
        "the next run looked him up before it walked, though it too was stopped"
    );
}
