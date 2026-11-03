//! The walk has several requests out at once (history pages of
//! different channels, threads, files) and stores what a walk making
//! one at a time stores.

use std::path::Path;
use std::time::Duration;

use datalib_etl::stop::StopFlag;
use datalib_etl_web::interrupt::Rig;
use datalib_etl_web::playback::{self, Playback};

use crate::every_cut::{workspace, Slack};
use crate::support::one_at_a_time;

/// A first page of `#bridge` and of `#engineering`, as their URLs start.
const TWO_CHANNELS: [&str; 2] = [
    "conversations.history?channel=C_BRIDGE&",
    "conversations.history?channel=C_ENG&",
];

/// One whole run of the every-cut workspace into `dir`, answered by
/// `tape`, and what it stored.
async fn mirror(dir: &Path, tape: Playback, alone: bool) -> String {
    let db = Slack.open(dir).await.unwrap();
    let stop = StopFlag::new();
    let download = Slack.download(&db, stop.clone());
    let ran = if alone {
        Ok(playback::scope(tape, one_at_a_time(stop, download)).await)
    } else {
        tokio::time::timeout(Duration::from_secs(60), playback::scope(tape, download)).await
    };
    Slack.seal(db).await.unwrap();
    ran.expect("the history of #bridge and #engineering was never asked for together")
        .unwrap();
    Slack.contents(dir).await.unwrap()
}

/// History was read one page at a time, one channel after another. The
/// tape answers the first page of `#bridge` and of `#engineering` only
/// once both have been asked for, so a walk making one request at a time
/// never finishes, and the deadline names the two.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_channels_history_goes_out_together_and_stores_what_one_at_a_time_does() {
    let tape = workspace();
    let scratch = tempfile::tempdir().unwrap();
    let alone = mirror(
        &scratch.path().join("alone"),
        Playback::at(&tape.playback),
        true,
    )
    .await;
    let together = mirror(
        &scratch.path().join("together"),
        Playback::at(&tape.playback).meet(&TWO_CHANNELS),
        false,
    )
    .await;
    let differs = alone.lines().zip(together.lines()).find(|(a, t)| a != t);
    assert!(
        alone == together,
        "requests made together stored something else; the first line that differs, \
         one at a time then together: {differs:?}"
    );
}
