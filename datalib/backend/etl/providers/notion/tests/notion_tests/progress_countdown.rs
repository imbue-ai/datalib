//! The Manage screen's ETA is a step's queue (the total it announced
//! less what it counted done) over the pace it counts things done.
//! Notion announced no total, so a sync showed a count and no ETA.
//! These tests replay the runner's arithmetic over a run's events.

use std::sync::{Arc, Mutex};

use datalib_etl::progress::{Progress, ProgressSink};
use datalib_etl_notion::ingest::FetchOptions;
use serde_json::json;
use tempfile::tempdir;

use crate::support::*;

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
    /// The `queued` the runner would publish after each event: `done`
    /// sums every increment, `total` is the last announced, and nothing
    /// is published while no total has been.
    fn queued_series(&self) -> Vec<u64> {
        let (mut done, mut total) = (0u64, None);
        let mut out = Vec::new();
        for ev in self.events.lock().unwrap().iter() {
            match ev {
                Ev::Length(t) => total = *t,
                Ev::Inc(d) => done += d,
            }
            if let Some(t) = total {
                out.push(t.saturating_sub(done));
            }
        }
        out
    }

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

/// With everything announced counted done, a queue that stays at zero
/// once it gets there never read zero while work was left. The empty
/// loops at the end re-announce the same total, hence several zeros.
fn assert_zero_only_once_done(series: &[u64]) {
    let first_zero = series.iter().position(|q| *q == 0);
    assert!(
        first_zero.is_some_and(|i| series[i..].iter().all(|q| *q == 0)),
        "the queue read zero with work left: {series:?}"
    );
}

async fn run_recorded(roots: &[&str]) -> Recorder {
    let d = tempdir().unwrap();
    let (tape, store) = (d.path().join("tape"), d.path().join("s.doltlite_db"));
    serve_page(&tape, BRIDGE, EDITED, "");
    serve_page(&tape, SICKBAY, EDITED, "");
    serve_search(
        &tape,
        None,
        json!([page(BRIDGE, EDITED), page(SICKBAY, EDITED)]),
        None,
    );
    let recorder = Recorder::default();
    run_with(&tape, &store, |o| FetchOptions {
        subtree_pages: roots.iter().map(|r| r.to_string()).collect(),
        progress: Progress::new(Arc::new(recorder.clone())),
        ..o
    })
    .await
    .unwrap();
    recorder
}

/// A search walk lists two pages; each owes a body and a comment
/// listing, and all four are in the queue before the first is fetched.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_search_run_queues_each_listed_pages_body_and_comments() {
    let recorder = run_recorded(&[]).await;
    assert_eq!(recorder.announced_and_done(), (4, 4));
    let series = recorder.queued_series();
    assert_eq!(
        series.first(),
        Some(&4),
        "the listing announces what it owes: {series:?}"
    );
    assert_zero_only_once_done(&series);
}

/// A roots walk also counts each page object it asks for, and the
/// bodies the listing foresaw are not counted again by their loop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_roots_run_counts_objects_bodies_and_comments_once_each() {
    let recorder = run_recorded(&[BRIDGE, SICKBAY]).await;
    assert_eq!(recorder.announced_and_done(), (6, 6));
    let series = recorder.queued_series();
    assert_zero_only_once_done(&series);
}
