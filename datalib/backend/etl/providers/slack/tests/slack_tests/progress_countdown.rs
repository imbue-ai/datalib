//! "N queued" on the Manage screen is the DAG runner subtracting a
//! step's progress increments from the last total that step announced,
//! and its ETA is that queue over the pace increments arrive. These
//! tests replay the runner's own arithmetic over the events a run emits.
//!
//! Two ways it has gone wrong: one bar per channel, each announcing its
//! own size, pinned the count at zero after the first channel; and
//! counting messages, which Slack names no total for, made each page
//! done the moment it was known, so a run with most of its channels
//! still to walk read "1s left".

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};

use datalib_etl::progress::{Progress, ProgressSink};
use datalib_etl_slack::ingest::FetchOptions;
use datalib_etl_slack::recorded::{record_call, record_workspace};
use datalib_etl_web::http::{HttpRequest, HttpResponse, HttpService};
use datalib_etl_web::synthesize::write_fixture;
use serde_json::{json, Value};

use crate::support::{fetch_into, Tree};

const CHANNELS: [&str; 3] = ["C1", "C2", "C3"];
const PER_CHANNEL: usize = 4;
/// `C1`'s oldest message starts a thread with these replies.
const THREAD_ROOT: &str = "1735689600.000100";
const REPLIES: [&str; 2] = ["1735689700.000001", "1735689700.000002"];
/// `C2`'s oldest message carries this file.
const FILE_URL: &str = "https://files.slack.com/files-pri/T1-F_ISOLINEAR/download/isolinear.png";
/// What a run has to do, one tick each: walk every channel, read the
/// thread, fetch the file.
const UNITS: u64 = CHANNELS.len() as u64 + 1 + 1;

/// The progress events a step emits, in order. The runner relabels
/// child bars back to the step, so one recorder stands in for all of
/// them — which is exactly the collision this test is about.
#[derive(Clone, Copy, Debug)]
enum Ev {
    Length(Option<u64>),
    Inc(u64),
}

#[derive(Default, Clone)]
struct Recorder {
    events: Arc<Mutex<Vec<Ev>>>,
}

impl ProgressSink for Recorder {
    fn set_length(&self, total: Option<u64>) {
        self.events.lock().unwrap().push(Ev::Length(total));
    }
    fn inc(&self, delta: u64) {
        self.events.lock().unwrap().push(Ev::Inc(delta));
    }
}

impl Recorder {
    /// The `queued` series the runner would have published, one entry
    /// per progress event. Mirrors `RunStoreSink::publish_sugar`:
    /// `done` accumulates every increment, `total` is replaced
    /// wholesale, and `queued` is their saturating difference — with no
    /// entry at all while no total has been announced.
    fn queued_series(&self) -> Vec<u64> {
        let mut done: u64 = 0;
        let mut total: Option<u64> = None;
        let mut out = Vec::new();
        for ev in self.events.lock().unwrap().iter() {
            match ev {
                Ev::Length(t) => total = *t,
                Ev::Inc(d) => done = done.saturating_add(*d),
            }
            if let Some(t) = total {
                out.push(t.saturating_sub(done));
            }
        }
        out
    }

    /// The most the run ever announced, and everything it counted done.
    fn announced_and_done(&self) -> (u64, u64) {
        let events = self.events.lock().unwrap();
        let announced = events
            .iter()
            .filter_map(|ev| match ev {
                Ev::Length(t) => *t,
                Ev::Inc(_) => None,
            })
            .max()
            .unwrap_or(0);
        let done = events
            .iter()
            .map(|ev| match ev {
                Ev::Inc(d) => *d,
                Ev::Length(_) => 0,
            })
            .sum();
        (announced, done)
    }
}

/// A run over three channels, a thread and a file, with its events.
async fn run() -> Recorder {
    let t = Tree::new();
    record_workspace(&t.api, &CHANNELS, messages).unwrap();
    record_thread(&t.api);
    t.serve();
    serve_file(&t.playback);

    let recorder = Recorder::default();
    let result = fetch_into(&t.playback, &t.out, |o| FetchOptions {
        progress: Progress::new(Arc::new(recorder.clone())),
        media: true,
        ..o
    })
    .await;
    let summary = result.expect("slack fetch under playback");
    assert_eq!(
        (summary.messages, summary.replies),
        (CHANNELS.len() * PER_CHANNEL, REPLIES.len()),
        "every channel's messages and the thread's replies should have landed",
    );
    recorder
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_count_reaches_zero_once_at_the_end_not_after_the_first_channel() {
    let recorder = run().await;
    let series = recorder.queued_series();
    assert!(
        !series.is_empty(),
        "the export announced no total at all, so the Manage screen shows \
         a bare count with no remaining",
    );
    // Zero means "nothing left to do", so it may only appear once the
    // run really is done.
    let first_zero = series.iter().position(|q| *q == 0);
    assert_eq!(
        first_zero,
        Some(series.len() - 1),
        "\"N queued\" hit zero at index {first_zero:?} of {} and the run \
         kept going (series: {series:?})",
        series.len(),
    );
}

/// The regression behind a "1s left" on a download with most of its
/// channels still ahead: a message counted done the moment its page
/// arrived, so the pace was messages and the queue was channels.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_bar_counts_channels_threads_and_files_and_never_a_message() {
    let recorder = run().await;
    assert_eq!(
        recorder.announced_and_done(),
        (UNITS, UNITS),
        "one tick per channel walked, thread read and file fetched; \
         a message is none of them",
    );
}

/// Channel `i`'s page: [`PER_CHANNEL`] messages from Picard, `C1`'s
/// oldest a thread root and `C2`'s oldest carrying a file.
fn messages(i: usize, channel: &str) -> Value {
    (0..PER_CHANNEL)
        .map(|m| {
            let mut said = json!({
                "ts": format!("17356896{:02}.0001{:02}", i, m),
                "user": "U1",
                "text": format!("message {m} in {channel}"),
            });
            match (channel, m) {
                ("C1", 0) => {
                    said["thread_ts"] = json!(THREAD_ROOT);
                    said["reply_count"] = json!(REPLIES.len());
                    said["latest_reply"] = json!(REPLIES[REPLIES.len() - 1]);
                }
                ("C2", 0) => {
                    said["files"] = json!([{
                        "id": "F_ISOLINEAR",
                        "name": "isolinear.png",
                        "mimetype": "image/png",
                        "size": 4,
                        "url_private_download": FILE_URL,
                    }]);
                }
                _ => {}
            }
            said
        })
        .collect()
}

fn record_thread(api: &Path) {
    let mut thread = vec![
        json!({"ts": THREAD_ROOT, "user": "U1", "text": "message 0 in C1",
                                 "thread_ts": THREAD_ROOT}),
    ];
    thread.extend(REPLIES.iter().map(|ts| {
        json!({"ts": ts, "user": "U1", "text": format!("reply {ts}"), "thread_ts": THREAD_ROOT})
    }));
    record_call(
        api,
        "conversations.replies",
        json!({"channel": "C1", "ts": THREAD_ROOT, "limit": "200"}),
        json!({"ok": true, "has_more": false, "messages": thread}),
    )
    .unwrap();
}

fn serve_file(playback: &Path) {
    write_fixture(
        playback,
        &HttpRequest::get(HttpService::Slack, FILE_URL),
        &HttpResponse {
            status: 200,
            headers: BTreeMap::new(),
            body: b"\x89PNG".to_vec(),
            duration_ms: 0,
        },
    )
    .unwrap();
}
