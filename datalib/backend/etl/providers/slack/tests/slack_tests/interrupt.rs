//! A stop requested mid-run ends the walk at the next channel boundary:
//! the channel in flight finishes, no further channel starts, and the run
//! returns `Ok` with what it has — the shape `finish` then commits as the
//! last seal. The flag is raised from the progress sink's per-channel
//! tick, so the test does not depend on timing.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use datalib_etl::control::DownloadControl;
use datalib_etl::progress::{Progress, ProgressSink};
use datalib_etl::stop::StopFlag;
use datalib_etl_slack::ingest::{db_path_for, FetchOptions};
use datalib_etl_slack::recorded::record_workspace;
use serde_json::json;

use crate::support::{channels_with_messages, checked_at, fetch_into, Tree};

/// Raises the stop on the download's first progress tick — what the SIGINT
/// handler does, at a point the test controls. The first tick now lands
/// part-way through the first channel rather than at its end, so this
/// asserts the stronger thing: the channel already in flight still
/// finishes, and none after it starts.
struct StopOnFirstChannel(StopFlag);

impl ProgressSink for StopOnFirstChannel {
    fn inc(&self, _delta: u64) {
        self.0.request();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stop_ends_the_walk_at_the_next_channel_boundary() {
    let t = Tree::new();
    record_workspace(&t.api, &["C1", "C2", "C3"], |i, c| {
        json!([{"ts": format!("1735689600.0001{i:02}"), "user": "U1", "text": format!("in {c}")}])
    })
    .unwrap();
    t.serve();

    let stop = StopFlag::new();
    let summary = fetch_into(&t.playback, &t.out, |o| FetchOptions {
        progress: Progress::new(Arc::new(StopOnFirstChannel(stop.clone()))),
        control: DownloadControl {
            stop: stop.clone(),
            ..Default::default()
        },
        ..o
    })
    .await;
    summary.expect("a stopped run is a shorter run, not a failed one");

    let walked = channels_with_messages(&t.out);
    assert_eq!(
        walked.len(),
        1,
        "one channel finished, none started after the stop: {walked:?}"
    );
    assert!(stop.requested());

    // The channels the run never started have no coverage, which is all
    // the next run needs to walk them.
    let reader = datalib_pin::open_reader(&db_path_for(&t.out))
        .await
        .unwrap();
    let covered: Vec<String> = sqlx::query_scalar("SELECT scope FROM coverage ORDER BY scope")
        .fetch_all(&reader)
        .await
        .unwrap();
    let status: String =
        sqlx::query_scalar("SELECT status FROM sync_runs ORDER BY run_id DESC LIMIT 1")
            .fetch_one(&reader)
            .await
            .unwrap();
    reader.close().await;
    assert_eq!(covered.len(), 1, "{covered:?}");
    assert_eq!(
        status, "partial",
        "it did not read everything it set out to"
    );
    assert_eq!(
        checked_at(&t.out)
            .await
            .keys()
            .cloned()
            .collect::<BTreeSet<_>>(),
        walked,
        "the channel it finished is current; the ones it never started are not"
    );

    fetch_into(&t.playback, &t.out, |o| o).await.unwrap();
    assert_eq!(channels_with_messages(&t.out).len(), 3);
}

/// The runner keeps what a step's queue still holds as it ends as the
/// step's `owed`, so a stopped run's bar has to leave on it exactly what
/// the next run walks: here the two channels it never started.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_stopped_run_leaves_on_its_queue_the_channels_it_did_not_walk() {
    let t = Tree::new();
    record_workspace(&t.api, &["C1", "C2", "C3"], |i, c| {
        json!([{"ts": format!("1735689600.0001{i:02}"), "user": "U1", "text": format!("in {c}")}])
    })
    .unwrap();
    t.serve();

    let stop = StopFlag::new();
    let bar = StopAndCount {
        stop: stop.clone(),
        ..Default::default()
    };
    fetch_into(&t.playback, &t.out, |o| FetchOptions {
        progress: Progress::new(Arc::new(bar.clone())),
        control: DownloadControl {
            stop: stop.clone(),
            ..Default::default()
        },
        ..o
    })
    .await
    .expect("a stopped run is a shorter run");

    let left = bar.total.lock().unwrap().unwrap_or(0) - bar.done.load(Ordering::SeqCst);
    let unwalked = 3 - channels_with_messages(&t.out).len() as u64;
    assert_eq!((left, unwalked), (2, 2));
}

/// Stops the run on its first tick, as [`StopOnFirstChannel`] does, and
/// keeps the total it announced and what it counted done.
#[derive(Clone, Default)]
struct StopAndCount {
    stop: StopFlag,
    total: Arc<Mutex<Option<u64>>>,
    done: Arc<AtomicU64>,
}

impl ProgressSink for StopAndCount {
    fn set_length(&self, total: Option<u64>) {
        *self.total.lock().unwrap() = total;
    }
    fn inc(&self, delta: u64) {
        self.done.fetch_add(delta, Ordering::SeqCst);
        self.stop.request();
    }
}
