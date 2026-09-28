//! A stop requested mid-run ends the walk at the next channel boundary:
//! the channel in flight finishes, no further channel starts, and the run
//! returns `Ok` with what it has — the shape `finish` then commits as the
//! last seal. The flag is raised from the progress sink's per-channel
//! tick, so the test does not depend on timing.

use std::sync::Arc;

use datalib_etl::control::DownloadControl;
use datalib_etl::progress::{Progress, ProgressSink};
use datalib_etl::stop::StopFlag;
use datalib_etl_slack::ingest::{db_path_for, FetchOptions};
use datalib_etl_slack::recorded::record_workspace;
use serde_json::json;

use crate::support::{channels_with_messages, fetch_into, Tree};

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
    let summary = fetch_into(&t.out, |o| FetchOptions {
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

    // The run did not walk every channel the config names, so it must not
    // have recorded the config as satisfied: the next run has to reach
    // the channels this one never started, exactly as after a widened
    // filter.
    let reader = datalib_pin::open_reader(&db_path_for(&t.out))
        .await
        .unwrap();
    let recorded = datalib_etl::scope_config::load(&reader, "slack:download")
        .await
        .unwrap();
    reader.close().await;
    assert!(
        recorded.is_none(),
        "an interrupted run recorded its scope config as satisfied: {recorded:?}"
    );
}
