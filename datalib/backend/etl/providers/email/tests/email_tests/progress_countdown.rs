//! The Manage screen's "N queued" chip is the DAG runner subtracting a
//! step's increments from the last total that step announced. A download
//! that never announces one gets a bare count instead — no remaining, no
//! countdown — which is what Gmail and Fastmail showed while Slack, which
//! does announce one, counted down.
//!
//! So the assertion that matters is not "a total was announced" but
//! "the last total announced equals what the run counted" — a bar that
//! announces a stale or partial total reads as stuck just as badly as
//! one that announces none. Both tests have been watched failing: with
//! the announcement removed the series ends at 0 of 3, and with the
//! skipped-id tick removed a re-walk ends at 0 of 3 the other way.

use std::path::Path;
use std::sync::Arc;

use datalib_etl::progress::Progress;
use datalib_etl_email::ingest::gmail_api::{self, FetchOptions};
use serde_json::json;

use crate::support::{
    gmail_get_url, gmail_list_url, gmail_message, inbox_label, put_gmail, put_gmail_account,
    Mirror, Recorder,
};

const IDS: [&str; 3] = ["18c9f2a1b2c3d401", "18c9f2a1b2c3d402", "18c9f2a1b2c3d403"];

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_gmail_walk_announces_a_total_so_the_chip_can_count_down() {
    let m = Mirror::new();
    write_fixtures(&m.playback);

    let recorder = Recorder::default();
    let summary = m
        .run(|db| {
            let mut opts = FetchOptions::new(db);
            opts.progress = Progress::new(Arc::new(recorder.clone()));
            gmail_api::fetch(opts)
        })
        .await;

    let summary = summary.expect("gmail fetch under playback");
    assert_eq!(summary.emails_upserted, IDS.len());

    let announced = recorder.announcements();
    let last = *announced.last().expect("a total was announced");
    assert_eq!(
        last,
        IDS.len() as u64,
        "the last total announced was {last}, not the {} messages the \
         walk listed; the chip would not reach zero (series: {announced:?})",
        IDS.len(),
    );
    assert_eq!(
        recorder.final_done(),
        IDS.len() as u64,
        "every listed id must tick the bar, or it stalls short of its total",
    );
}

/// A re-walk of a mailbox already mirrored fetches nothing: every id is
/// skipped before `messages.get`. The bar must still reach its total,
/// or a routine incremental run leaves the chip pinned near full.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_walk_that_fetches_nothing_still_reaches_zero_queued() {
    let m = Mirror::new();
    write_fixtures(&m.playback);

    // First run mirrors everything.
    m.run(|db| gmail_api::fetch(FetchOptions::new(db)))
        .await
        .expect("first gmail fetch");

    // Second run: `full_resync`, so it walks the same ids again and
    // skips every one of them as already held.
    let recorder = Recorder::default();
    let summary = m
        .run(|db| {
            let mut opts = FetchOptions::new(db);
            opts.config.full_resync = true;
            opts.progress = Progress::new(Arc::new(recorder.clone()));
            gmail_api::fetch(opts)
        })
        .await;

    let summary = summary.expect("second gmail fetch under playback");
    assert_eq!(
        summary.messages_already_had,
        IDS.len(),
        "the second run should have skipped every id: {summary:?}",
    );
    let announced = recorder.announcements();
    let last = *announced.last().expect("a total was announced");
    assert_eq!(
        recorder.final_done(),
        last,
        "the bar stopped at {} of {last}: a skipped id never ticked it, \
         so \"N queued\" stays pinned near full for the whole run",
        recorder.final_done(),
    );
}

fn write_fixtures(playback: &Path) {
    put_gmail_account(playback, "1000", json!([inbox_label()]));
    // One unrestricted walk, one page. `resultSizeEstimate` is what the
    // bar counts down from; Gmail sends it on every page.
    put_gmail(
        playback,
        &gmail_list_url(&[]),
        &json!({
            "messages": IDS.iter().map(|id| json!({ "id": id })).collect::<Vec<_>>(),
            "resultSizeEstimate": IDS.len(),
        }),
    );
    for id in IDS {
        put_gmail(
            playback,
            &gmail_get_url(id),
            &gmail_message(id, &["INBOX"], id),
        );
    }
}
