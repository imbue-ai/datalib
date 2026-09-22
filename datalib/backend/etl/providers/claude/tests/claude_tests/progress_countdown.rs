//! One conversation, one tick, against a total that never shrinks.
//!
//! The DAG runner sums a step's progress increments into `done` and
//! computes the "N queued" the Manage screen shows as `total - done`,
//! against whichever length the step announced last. This download used
//! to break that twice over: it gave each org a bar of its own that
//! announced that org's size, and it ticked *both* that bar and the
//! outer one for the same conversation. Against two orgs of one chat
//! each, `done` reached 4 for 2 conversations and the count read zero
//! from the third event onward.
//!
//! Pass 1 of the download carries a comment about exactly this hazard —
//! "a length reset per org makes the bar jump backwards" — which the
//! per-org bar in Pass 2 then reintroduced. This test is what keeps the
//! comment honest.

use std::fs;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use datalib_etl::http::PLAYBACK_ENV;
use datalib_etl::progress::{Progress, ProgressSink};
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl::synthesize::Synthesizer;
use datalib_etl_claude::ingest::{db::db_path_for, fetch, FetchOptions, RawDb};
use datalib_etl_claude::synthesize::ClaudeSynth;
use serde_json::json;
use tempfile::tempdir;

/// One conversation in each of two orgs: the smallest shape that shows
/// a per-org bar clobbering the run's total.
const CONVERSATIONS: usize = 2;

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
    /// The `queued` series the runner would have published, mirroring
    /// `RunStoreSink::publish_sugar`: `done` accumulates, `total` is
    /// replaced wholesale, `queued` is their saturating difference, and
    /// nothing is published until a total has been announced.
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

    fn final_done(&self) -> u64 {
        self.events
            .lock()
            .unwrap()
            .iter()
            .map(|e| match e {
                Ev::Inc(d) => *d,
                Ev::Length(_) => 0,
            })
            .sum()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn each_conversation_ticks_once_and_the_count_reaches_zero_only_at_the_end() {
    let d = tempdir().unwrap();
    let api = d.path().join("input_snapshot");
    let playback = d.path().join("playback");
    let out_db = d.path().join("out_snapshot.doltlite_db");
    fs::create_dir_all(&api).unwrap();

    fs::write(
        api.join("conversations.json"),
        serde_json::to_vec_pretty(&json!([
            {
                "uuid": "c1", "name": "First", "updated_at": "2025-01-02T00:00:00Z",
                "organization_uuid": "org-a", "account": {"uuid": "acct-1"},
                "chat_messages": [], "_source": {"via": "claude.ai/api", "org_uuid": "org-a"},
            },
            {
                "uuid": "c2", "name": "Second", "updated_at": "2025-01-01T00:00:00Z",
                "organization_uuid": "org-b", "account": {"uuid": "acct-1"},
                "chat_messages": [], "_source": {"via": "claude.ai/api", "org_uuid": "org-b"},
            },
        ]))
        .unwrap(),
    )
    .unwrap();
    fs::write(
        api.join("users.json"),
        serde_json::to_vec_pretty(&json!([{"uuid": "acct-1"}])).unwrap(),
    )
    .unwrap();

    ClaudeSynth::new(&api).synthesize(&playback).unwrap();
    std::env::set_var(PLAYBACK_ENV, &playback);

    let recorder = Recorder::default();
    let db = RawDb::open(&db_path_for(&out_db)).await.unwrap();
    let summary = fetch(FetchOptions {
        export_dir: Some(api.clone()),
        overlap: 0,
        sleep_between: Duration::ZERO,
        conv_uuids: Vec::new(),
        progress: Progress::new(Arc::new(recorder.clone())),
        ..FetchOptions::new(db.clone())
    })
    .await;
    db.commit_all("test").await.unwrap();
    db.close().await;
    std::env::remove_var(PLAYBACK_ENV);

    let summary = summary.expect("claude fetch under playback");
    assert_eq!(summary.fetched, CONVERSATIONS);

    assert_eq!(
        recorder.final_done(),
        CONVERSATIONS as u64,
        "the run ticked {} times for {CONVERSATIONS} conversations — a \
         conversation must advance the count once, not once per bar that \
         happens to be watching it",
        recorder.final_done(),
    );

    let series = recorder.queued_series();
    let first_zero = series.iter().position(|q| *q == 0);
    assert_eq!(
        first_zero,
        Some(series.len() - 1),
        "\"N queued\" hit zero at index {first_zero:?} of {} and the run \
         kept going — every org after that one reads zero remaining \
         (series: {series:?})",
        series.len(),
    );
}
