//! Scenario tests for the supervisor loop's management of processes: real
//! step processes that do only what they are told (`tests/driver/main.rs`),
//! the loop run in-process, and a person syncing, stopping, pausing and
//! resuming through the store.
//!
//! What is asserted is the loop's own business: which processes it starts
//! and when, never two of a step at once, how each request ends, what each
//! step's record says, the Stopping state, backoff, pause and resume, a
//! config reloaded mid-sync, a consumer woken by its producer's seal and
//! handed the version the loop recorded for it. What a step writes, and
//! whether that is atomic, is not — that is storage's, and `etl`'s
//! `doltlite_interrupt_test` holds it.

mod harness;
mod walk;

use std::time::Duration;

use datalib_dag::supervisor::store::RequestOutcome;
use datalib_dag::supervisor::tick::StateKind;
use datalib_dag::{Event, RunState};
use harness::{reads, step, Harness, Options};

const A: &str = "a/ingest";
const B: &str = "b/ingest";
const C: &str = "c/render";

impl Harness {
    /// The version the loop has recorded for `step`'s tree.
    async fn version(&self, step: &str) -> Option<String> {
        self.record(step).await.version
    }

    /// Have `consumer` say what the loop started it against, and require
    /// it to be what the loop has recorded for `producer` now.
    async fn reads_current(&mut self, consumer: &str, producer: &str) -> String {
        let got = self.done(consumer, "reads").await;
        let want = self
            .version(producer)
            .await
            .expect("the producer has a version");
        assert_eq!(
            got,
            format!("reads {producer}={want}"),
            "{consumer} was handed another version"
        );
        want
    }

    async fn last_status(&self, step: &str) -> String {
        self.record(step)
            .await
            .last_run
            .map(|l| l.status)
            .unwrap_or_default()
    }
}

/// The baseline: a step told to finish does, reports its version, its
/// request closes done, and its record says it succeeded with that version.
#[tokio::test(flavor = "multi_thread")]
async fn a_step_does_what_it_is_told_and_its_request_closes_done() {
    let mut h = Harness::new(&[step(A)]).await;
    let r = h.sync(&[A]).await;
    h.expect(A, "started").await;
    h.done(A, "write f hello").await;
    h.done(A, "ok v1").await;
    assert_eq!(h.closed(&r).await, Some(RequestOutcome::Done));
    assert_eq!(h.last_status(A).await, "succeeded");
    assert!(h.version(A).await.unwrap().ends_with("v1"));
    h.finish().await;
}

/// Stopped mid-stall, a step reads stopped and its request closes stopped;
/// the next sync starts it afresh.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_mid_stall_reads_stopped_and_the_next_sync_runs_it() {
    let mut h = Harness::new(&[step(A)]).await;
    let r = h.sync(&[A]).await;
    h.expect(A, "started").await;
    h.done(A, "stall").await;
    h.stop(&r).await;
    h.expect(A, "stopped").await;
    assert_eq!(h.closed(&r).await, Some(RequestOutcome::Stopped));
    h.until("a's run to read stopped", |rec, _| {
        rec.steps
            .get(A)?
            .last_run
            .as_ref()
            .filter(|l| l.status == "stopped")
            .map(|_| ())
    })
    .await;

    let again = h.sync(&[A]).await;
    h.expect(A, "started").await;
    h.done(A, "ok").await;
    assert_eq!(h.closed(&again).await, Some(RequestOutcome::Done));
    h.finish().await;
}

/// A step deaf to its stop is killed at the grace, and its request closes
/// stopped all the same.
#[tokio::test(flavor = "multi_thread")]
async fn a_step_deaf_to_its_stop_is_killed_at_the_grace() {
    let mut h = Harness::new(&[step(A)]).await;
    let r = h.sync(&[A]).await;
    h.expect(A, "started").await;
    h.done(A, "on_stop ignore").await;
    h.done(A, "spin").await;
    h.stop(&r).await;
    h.expect(A, "ignoring a stop").await;
    assert_eq!(h.closed(&r).await, Some(RequestOutcome::Stopped));
    h.until("a to be killed and read stopped", |rec, _| {
        rec.steps
            .get(A)?
            .last_run
            .as_ref()
            .filter(|l| l.status == "stopped")
            .map(|_| ())
    })
    .await;
    h.finish().await;
}

/// Pausing a running step stops it; resuming lets the next sync run it.
#[tokio::test(flavor = "multi_thread")]
async fn a_paused_step_stops_and_a_resumed_one_runs_again() {
    let mut h = Harness::new(&[step(A)]).await;
    let r = h.sync(&[A]).await;
    h.expect(A, "started").await;
    h.done(A, "stall").await;
    h.pause(A).await;
    h.expect(A, "stopped").await;
    h.closed(&r).await;

    h.resume(A).await;
    let again = h.sync(&[A]).await;
    h.expect(A, "started").await;
    h.done(A, "ok").await;
    assert_eq!(h.closed(&again).await, Some(RequestOutcome::Done));
    h.finish().await;
}

/// A consumer runs when what it reads moves, is handed the version the
/// loop recorded, and does not run again for a version that did not move.
#[tokio::test(flavor = "multi_thread")]
async fn a_consumer_runs_only_when_its_producers_version_moves() {
    let mut h = Harness::new(&[step(A), reads(C, &[A])]).await;
    let r = h.sync(&[A]).await;
    h.expect(A, "started").await;
    h.done(A, "ok v1").await;
    h.expect(C, "started").await;
    h.reads_current(C, A).await;
    h.done(C, "ok").await;
    assert_eq!(h.closed(&r).await, Some(RequestOutcome::Done));

    let same = h.sync(&[A]).await;
    h.expect(A, "started").await;
    h.done(A, "ok v1").await;
    assert_eq!(h.closed(&same).await, Some(RequestOutcome::Done));
    assert_eq!(
        h.pending_acks(C),
        Vec::<String>::new(),
        "c ran for an unmoved version"
    );

    let moved = h.sync(&[A]).await;
    h.expect(A, "started").await;
    h.done(A, "ok v2").await;
    h.expect(C, "started").await;
    assert!(h.reads_current(C, A).await.ends_with("v2"));
    h.done(C, "ok").await;
    assert_eq!(h.closed(&moved).await, Some(RequestOutcome::Done));
    h.finish().await;
}

/// A consumer of a streaming producer runs on each seal, handed that
/// seal's version, while the producer runs on; its queue rises with each
/// seal's rows and drains as it reads them; and the producer finishing on
/// the version it last sealed runs nothing more.
#[tokio::test(flavor = "multi_thread")]
async fn a_consumer_runs_on_each_seal_while_its_producer_streams() {
    let mut h = Harness::new(&[step(A), reads(C, &[A])]).await;
    let r = h.sync(&[A]).await;
    h.expect(A, "started").await;
    h.done(A, "streams").await;
    h.done(A, "seal v1 2").await;

    h.expect(C, "started").await;
    h.event("the consumer's queue to show the seal", |e| {
        matches!(e, Event::Metric { step, name, labels, value }
            if step == C && name == "queued" && labels.get("from").map(String::as_str) == Some(A) && *value == 2)
    })
    .await;
    assert!(h.reads_current(C, A).await.ends_with("v1"));
    h.done(C, "ok").await;
    h.event("the queue to drain", |e| {
        matches!(e, Event::Metric { step, name, value, .. } if step == C && name == "queued" && *value == 0)
    })
    .await;

    h.done(A, "seal v2 3").await;
    h.expect(C, "started").await;
    assert!(h.reads_current(C, A).await.ends_with("v2"));
    h.done(C, "ok").await;
    h.done(A, "ok v2").await;
    assert_eq!(h.closed(&r).await, Some(RequestOutcome::Done));
    assert_eq!(
        h.pending_acks(C),
        Vec::<String>::new(),
        "c ran again for the version it had read"
    );
    h.finish().await;
}

/// Stopping the sync of a streaming producer stops the consumer its seal
/// woke too: both were run for that request, and it is gone.
#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_streaming_producer_stops_its_consumer_too() {
    let mut h = Harness::new(&[step(A), reads(C, &[A])]).await;
    let r = h.sync(&[A]).await;
    h.expect(A, "started").await;
    h.done(A, "streams").await;
    h.done(A, "seal v1").await;
    h.expect(C, "started").await;
    h.stop(&r).await;
    h.expect(A, "stopped").await;
    h.expect(C, "stopped").await;
    assert_eq!(h.closed(&r).await, Some(RequestOutcome::Stopped));
    for s in [A, C] {
        let what = format!("{s}'s run to read stopped");
        h.until(&what, |rec, _| {
            rec.steps
                .get(s)?
                .last_run
                .as_ref()
                .filter(|l| l.status == "stopped")
                .map(|_| ())
        })
        .await;
    }
    h.finish().await;
}

/// Pausing a streaming producer stops it but not the consumer it woke,
/// which is still wanted and has a sealed version to read; it finishes,
/// and the request closes with the producer paused.
#[tokio::test(flavor = "multi_thread")]
async fn pausing_a_streaming_producer_leaves_its_consumer_to_finish() {
    let mut h = Harness::new(&[step(A), reads(C, &[A])]).await;
    let r = h.sync(&[A]).await;
    h.expect(A, "started").await;
    h.done(A, "streams").await;
    h.done(A, "seal v1").await;
    h.expect(C, "started").await;
    h.pause(A).await;
    h.expect(A, "stopped").await;
    assert!(h.reads_current(C, A).await.ends_with("v1"));
    h.done(C, "ok").await;
    h.closed(&r).await;
    assert_eq!(h.last_status(C).await, "succeeded");
    h.finish().await;
}

/// A paused consumer is not started by its producer's seals; resumed, it
/// runs on the next sync against what the producer last published.
#[tokio::test(flavor = "multi_thread")]
async fn a_paused_consumer_waits_out_its_producers_seals() {
    let mut h = Harness::new(&[step(A), reads(C, &[A])]).await;
    h.pause(C).await;
    let r = h.sync(&[A]).await;
    h.expect(A, "started").await;
    h.done(A, "streams").await;
    h.done(A, "seal v1").await;
    h.done(A, "seal v2").await;
    h.done(A, "ok v2").await;
    h.closed(&r).await;
    assert_eq!(
        h.pending_acks(C),
        Vec::<String>::new(),
        "a paused consumer started"
    );

    h.resume(C).await;
    let again = h.sync(&[A]).await;
    h.expect(A, "started").await;
    h.done(A, "ok v2").await;
    h.expect(C, "started").await;
    assert!(h.reads_current(C, A).await.ends_with("v2"));
    h.done(C, "ok").await;
    assert_eq!(h.closed(&again).await, Some(RequestOutcome::Done));
    h.finish().await;
}

/// The calendars of 2026-09-24: a source added to the config while another
/// syncs starts beside it, not behind it.
#[tokio::test(flavor = "multi_thread")]
async fn a_source_added_mid_sync_starts_beside_the_one_running() {
    let mut h = Harness::new(&[step(A)]).await;
    let first = h.sync(&[A]).await;
    h.expect(A, "started").await;

    h.set_config(&[step(A), step(B)]);
    let second = h.sync(&[B]).await;
    h.expect(B, "started").await;
    h.done(B, "ok").await;
    assert_eq!(h.closed(&second).await, Some(RequestOutcome::Done));
    assert_eq!(h.store.request(&first).await.unwrap().unwrap().closed, None);

    h.done(A, "ok").await;
    assert_eq!(h.closed(&first).await, Some(RequestOutcome::Done));
    h.finish().await;
}

/// What a step counts reaches the loop as it counts it: a metric as its
/// latest value, and a length and increments as progress.
#[tokio::test(flavor = "multi_thread")]
async fn what_a_step_counts_reaches_the_loop() {
    let mut h = Harness::new(&[step(A)]).await;
    let r = h.sync(&[A]).await;
    h.expect(A, "started").await;
    h.done(A, "metric api_requests 5 host=example").await;
    h.event("api_requests=5", |e| {
        matches!(e, Event::Metric { step, name, labels, value }
            if step == A && name == "api_requests" && *value == 5 && labels.get("host").map(String::as_str) == Some("example"))
    })
    .await;
    h.done(A, "progress_length 10").await;
    h.done(A, "progress_inc 3").await;
    h.event(
        "progress to arrive",
        |e| matches!(e, Event::ProgressInc { step, .. } if step == A),
    )
    .await;
    h.done(A, "ok").await;
    h.closed(&r).await;
    h.finish().await;
}

/// A transient failure is retried in the same request by a new process.
#[tokio::test(flavor = "multi_thread")]
async fn a_transient_failure_is_retried_by_a_new_process() {
    let mut h = Harness::new(&[step(A)]).await;
    let r = h.sync(&[A]).await;
    h.expect(A, "started 1").await;
    h.done(A, "fail transient").await;
    h.expect(A, "started 2").await;
    h.done(A, "ok").await;
    assert_eq!(h.closed(&r).await, Some(RequestOutcome::Done));
    h.finish().await;
}

/// Twenty syncs pressed while a source runs start it once more after it
/// ends, not twenty times, and never beside itself.
#[tokio::test(flavor = "multi_thread")]
async fn rapid_syncs_while_a_source_runs_start_it_once_more() {
    let mut h = Harness::new(&[step(A)]).await;
    let first = h.sync(&[A]).await;
    h.expect(A, "started").await;
    let mut rest = Vec::new();
    for _ in 0..20 {
        rest.push(h.sync(&[A]).await);
    }
    h.done(A, "ok").await;
    h.expect(A, "started").await;
    h.done(A, "ok").await;
    for r in std::iter::once(&first).chain(&rest) {
        assert_eq!(h.closed(r).await, Some(RequestOutcome::Done));
    }
    assert_eq!(h.pending_acks(A), Vec::<String>::new(), "a third start");
    h.finish().await;
}

/// Twenty syncs pressed in a burst on an idle source never run it two at
/// a time, and never more often than they were pressed: each run serves
/// every request open when it started. How many runs there are depends on
/// how fast each one ends, so that is not what is asserted.
#[tokio::test(flavor = "multi_thread")]
async fn rapid_syncs_on_an_idle_source_never_run_it_twice_at_once() {
    let mut h = Harness::new(&[step(A)]).await;
    // One per sync, the most a correct loop can use; whatever runs takes
    // the next.
    for _ in 0..20 {
        h.tell(A, "ok");
    }
    let mut all = Vec::new();
    for _ in 0..20 {
        all.push(h.sync(&[A]).await);
    }
    for r in &all {
        assert_eq!(h.closed(r).await, Some(RequestOutcome::Done));
    }
    let acks = h.pending_acks(A);
    let starts = acks.iter().filter(|a| a.starts_with("started")).count();
    let ends = acks.iter().filter(|a| a.starts_with("exiting 0")).count();
    assert!(
        (1..=20).contains(&starts),
        "{starts} runs for 20 syncs: {acks:?}"
    );
    assert_eq!(starts, ends, "{acks:?}");
    h.finish().await;
}

/// A stop that arrives while a failed step waits out its backoff ends
/// the wait, not the backoff: the step reads stopped at once and is not
/// tried again.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_during_a_retry_backoff_ends_it_at_once() {
    let hour = Options {
        backoff: Duration::from_secs(3600),
    };
    let mut h = Harness::with(&[step(A)], hour).await;
    let r = h.sync(&[A]).await;
    h.expect(A, "started 1").await;
    h.done(A, "fail transient").await;
    // Between attempts: no process, and still running.
    h.until("a to wait out its backoff as running", |rec, _| {
        (rec.steps.get(A)?.state == Some(StateKind::Running)).then_some(())
    })
    .await;
    h.stop(&r).await;
    assert_eq!(h.closed(&r).await, Some(RequestOutcome::Stopped));
    h.until("a's run to read stopped", |rec, _| {
        rec.steps
            .get(A)?
            .last_run
            .as_ref()
            .filter(|l| l.status == "stopped")
            .map(|_| ())
    })
    .await;
    assert_eq!(
        h.pending_acks(A),
        Vec::<String>::new(),
        "retried after the stop"
    );
    h.finish().await;
}

/// A pause during a backoff ends the wait the same way, and a resume
/// lets the next sync run the step afresh.
#[tokio::test(flavor = "multi_thread")]
async fn a_pause_during_a_retry_backoff_ends_it_and_a_resume_runs_it_again() {
    let hour = Options {
        backoff: Duration::from_secs(3600),
    };
    let mut h = Harness::with(&[step(A)], hour).await;
    let r = h.sync(&[A]).await;
    h.expect(A, "started 1").await;
    h.done(A, "fail transient").await;
    h.until("a to wait out its backoff as running", |rec, _| {
        (rec.steps.get(A)?.state == Some(StateKind::Running)).then_some(())
    })
    .await;
    h.pause(A).await;
    h.closed(&r).await;
    assert_eq!(
        h.pending_acks(A),
        Vec::<String>::new(),
        "retried after the pause"
    );

    h.resume(A).await;
    let again = h.sync(&[A]).await;
    h.expect(A, "started").await;
    h.done(A, "ok").await;
    assert_eq!(h.closed(&again).await, Some(RequestOutcome::Done));
    h.finish().await;
}

/// A step deaf to its stop reads Stopping — running, and saying so — for
/// as long as it lives, and Stopped once the grace has killed it.
#[tokio::test(flavor = "multi_thread")]
async fn a_step_deaf_to_its_stop_reads_stopping_then_stopped() {
    let mut h = Harness::new(&[step(A)]).await;
    let r = h.sync(&[A]).await;
    h.expect(A, "started").await;
    h.done(A, "on_stop ignore").await;
    h.done(A, "stall").await;
    h.stop(&r).await;
    h.expect(A, "ignoring a stop").await;
    // The loop records Stopping before it sends the signal the ack answers.
    let now = h.record(A).await;
    assert_eq!(now.state, Some(StateKind::Running), "{now:?}");
    assert!(
        now.state_detail
            .as_deref()
            .is_some_and(|d| d.starts_with("stopping")),
        "{now:?}"
    );
    h.until("a to be killed and read stopped", |rec, _| {
        rec.steps
            .get(A)?
            .last_run
            .as_ref()
            .filter(|l| l.status == "stopped")
            .map(|_| ())
    })
    .await;
    h.finish().await;
}

/// The loop sees how every process ended: an exit code, an abort, a kill.
#[tokio::test(flavor = "multi_thread")]
async fn the_loop_sees_how_each_process_ended() {
    for (instruction, code, signal) in [
        ("exit 3", Some(3), None),
        ("crash", None, Some(libc::SIGABRT)),
        ("kill", None, Some(libc::SIGKILL)),
    ] {
        let mut h = Harness::new(&[step(A)]).await;
        let r = h.sync(&[A]).await;
        h.expect(A, "started").await;
        h.done(A, instruction).await;
        let finish = h
            .event(
                "a's finish",
                |e| matches!(e, Event::StepFinish { step, .. } if step == A),
            )
            .await;
        let Event::StepFinish {
            status,
            exit_code,
            signal: sig,
            ..
        } = finish
        else {
            unreachable!()
        };
        assert_eq!(status, RunState::Failed, "{instruction}");
        assert_eq!((exit_code, sig), (code, signal), "{instruction}");
        assert_eq!(
            h.closed(&r).await,
            Some(RequestOutcome::Failed),
            "{instruction}"
        );
        h.finish().await;
    }
}

/// Seeded random walks through every way a step can end, under syncs,
/// stops, pauses and resumes, the invariant checked after each episode.
/// `HARNESS_SEED=<n>` replays one seed; `HARNESS_SEEDS=<n>` runs that
/// many.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn random_walks() {
    let env = |k: &str| std::env::var(k).ok().and_then(|v| v.parse::<u64>().ok());
    let seeds: Vec<u64> = match env("HARNESS_SEED") {
        Some(one) => vec![one],
        None => (0..env("HARNESS_SEEDS").unwrap_or(32)).collect(),
    };
    type Trail = std::sync::Arc<std::sync::Mutex<Vec<String>>>;
    let walks: Vec<(u64, Trail, tokio::task::JoinHandle<()>)> = seeds
        .into_iter()
        .map(|seed| {
            let trail = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
            let walk = walk::walk(seed, 6, trail.clone());
            let walk = async move {
                // Every wait inside has its own deadline; this one catches
                // a hang that is not a wait.
                if tokio::time::timeout(harness::DEADLINE * 3, walk)
                    .await
                    .is_err()
                {
                    panic!("hung past {:?}", harness::DEADLINE * 3);
                }
            };
            (seed, trail, tokio::spawn(walk))
        })
        .collect();
    let mut failed = Vec::new();
    for (seed, trail, walk) in walks {
        if let Err(e) = walk.await {
            let why = match e.try_into_panic() {
                Ok(p) => p
                    .downcast_ref::<String>()
                    .cloned()
                    .or_else(|| p.downcast_ref::<&str>().map(|s| s.to_string()))
                    .unwrap_or_default(),
                Err(e) => e.to_string(),
            };
            let trail = trail.lock().unwrap();
            let last = &trail[trail.len().saturating_sub(40)..];
            failed.push(format!(
                "seed {seed}: {why}\n  last of its trail:\n    {}",
                last.join("\n    ")
            ));
        }
    }
    assert!(
        failed.is_empty(),
        "{} walks failed:\n{}",
        failed.len(),
        failed.join("\n")
    );
}
