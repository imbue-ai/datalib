//! "N queued" on the Manage screen is the DAG runner subtracting a
//! step's progress increments from the last total that step announced.
//! `done` is summed across every bar the step makes; `total` is simply
//! whichever length came last. So a download that gives each unit of
//! work its own bar, each announcing its own size, pins `queued` at
//! zero from the second unit onward — `done` already carries the first
//! unit's work, and `saturating_sub` floors the difference.
//!
//! That is what this export used to do: one child bar per channel,
//! `set_length` with that channel's message count. The first channel
//! counted down and every channel after it read zero remaining. The
//! test replays the runner's own arithmetic over the events a run
//! emits and asserts the count reaches zero once, at the end.

use std::path::Path;
use std::sync::{Arc, Mutex};

use datalib_etl::http::PLAYBACK_ENV;
use datalib_etl::progress::{Progress, ProgressSink};
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl::synthesize::Synthesizer;
use datalib_etl_slack::ingest::{db_path_for, fetch, FetchOptions, RawDb};
use datalib_etl_slack::synthesize::SlackSynth;
use serde_json::{json, Value};
use tempfile::tempdir;
use tokio::sync::Mutex as AsyncMutex;

/// `PLAYBACK_ENV` is process-global, so this cannot run beside the other
/// playback tests.
static ENV_LOCK: AsyncMutex<()> = AsyncMutex::const_new(());

const TS_SINCE: &str = "1704067200.000000";
const CHANNEL_TYPES: &str = "public_channel,private_channel";
const CHANNELS: [&str; 3] = ["C1", "C2", "C3"];
const PER_CHANNEL: usize = 4;

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
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_count_reaches_zero_once_at_the_end_not_after_the_first_channel() {
    let _guard = ENV_LOCK.lock().await;
    let d = tempdir().unwrap();
    let api = d.path().join("input_raw");
    let playback = d.path().join("playback");
    let out = d.path().join("out_raw");

    write_fixtures(&api);
    SlackSynth::new(&api).synthesize(&playback).unwrap();
    std::env::set_var(PLAYBACK_ENV, &playback);

    let recorder = Recorder::default();
    let db = RawDb::open(&db_path_for(&out)).await.unwrap();
    let result = fetch(FetchOptions {
        channels: None,
        since: "2024-01-01".into(),
        refresh_window_days: 0,
        members_only: false,
        media: false,
        dms: false,
        progress: Progress::new(Arc::new(recorder.clone())),
        ..FetchOptions::new(db.clone())
    })
    .await;
    db.commit_all("test").await.unwrap();
    db.close().await;
    std::env::remove_var(PLAYBACK_ENV);

    let summary = result.expect("slack fetch under playback");
    assert_eq!(
        summary.messages,
        CHANNELS.len() * PER_CHANNEL,
        "every channel's messages should have landed",
    );

    let series = recorder.queued_series();
    assert!(
        !series.is_empty(),
        "the export announced no total at all, so the Manage screen shows \
         a bare count with no remaining",
    );
    // The whole point: zero means "nothing left to do", so it may only
    // appear once the run really is done.
    let first_zero = series.iter().position(|q| *q == 0);
    assert_eq!(
        first_zero,
        Some(series.len() - 1),
        "\"N queued\" hit zero at index {first_zero:?} of {} and the run \
         kept going — every channel after that one reads zero remaining \
         (series: {series:?})",
        series.len(),
    );
    // One tick per channel plus every message, which is what the run
    // announced; ending above zero would leave the chip stuck.
    assert_eq!(
        *series.last().unwrap(),
        0,
        "the run ended with work still announced but never counted \
         (series: {series:?})",
    );
}

// ── fixtures ────────────────────────────────────────────────────────

fn write_envelope(path: &Path, line: &Value) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut s = serde_json::to_string(line).unwrap();
    s.push('\n');
    std::fs::write(path, s).unwrap();
}

fn write_fixtures(api: &Path) {
    write_envelope(
        &api.join("raw_api/auth.test/run-1.jsonl"),
        &json!({
            "method": "auth.test", "params": {},
            "response": {"ok": true, "user_id": "U1", "team": "Enterprise", "team_id": "T1"},
        }),
    );
    write_envelope(
        &api.join("raw_api/users.list/run-1.jsonl"),
        &json!({
            "method": "users.list",
            "params": {"limit": "200"},
            "response": {"ok": true, "members": [
                {"id": "U1", "name": "picard", "real_name": "Jean-Luc Picard"},
            ]},
        }),
    );
    write_envelope(
        &api.join("raw_api/conversations.list/all.jsonl"),
        &json!({
            "method": "conversations.list",
            "params": {
                "exclude_archived": "true",
                "limit": "200",
                "types": CHANNEL_TYPES,
            },
            "response": {
                "ok": true,
                "channels": CHANNELS.iter().enumerate().map(|(i, id)| json!({
                    "id": id,
                    "name": format!("deck-{i}"),
                    "is_channel": true,
                    "is_member": true,
                    "is_archived": false,
                })).collect::<Vec<_>>(),
                "has_more": false,
            },
        }),
    );
    for (i, channel) in CHANNELS.iter().enumerate() {
        let messages: Vec<Value> = (0..PER_CHANNEL)
            .map(|m| {
                json!({
                    "ts": format!("17356896{:02}.0001{:02}", i, m),
                    "user": "U1",
                    "text": format!("message {m} in {channel}"),
                })
            })
            .collect();
        write_envelope(
            &api.join(format!("raw_api/conversations.history/{channel}.jsonl")),
            &json!({
                "method": "conversations.history",
                "params": {
                    "channel": channel,
                    "include_all_metadata": "true",
                    "inclusive": "true",
                    "limit": "200",
                    "oldest": TS_SINCE,
                },
                "response": {"ok": true, "messages": messages, "has_more": false},
            }),
        );
    }
}
