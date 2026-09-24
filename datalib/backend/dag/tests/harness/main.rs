//! Scenario tests for the supervisor loop: real step processes that do
//! only what they are told (`tests/driver/main.rs`), the loop run
//! in-process, and a person syncing, stopping, pausing and resuming
//! through the store. After every interruption the same invariant holds:
//! an acknowledged instruction happened, an unacknowledged one left no
//! trace, and what did not happen happens on the next sync.

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

/// The baseline: a step told to write and finish does, its request closes
/// done, and its record says it succeeded.
#[tokio::test(flavor = "multi_thread")]
async fn a_step_does_what_it_is_told_and_its_request_closes_done() {
    let mut h = Harness::new(&[step(A)]).await;
    let r = h.sync(&[A]).await;
    h.expect(A, "started").await;
    h.done(A, "write f hello").await;
    h.done(A, "ok").await;
    assert_eq!(h.closed(&r).await, Some(RequestOutcome::Done));
    h.check_files(A);
    assert_eq!(h.record(A).await.last_run.unwrap().status, "succeeded");
    h.finish().await;
}

/// Stopped mid-stall, a step keeps what it acknowledged and nothing else;
/// the next sync does the rest.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_keeps_what_was_done_and_the_next_sync_does_the_rest() {
    let mut h = Harness::new(&[step(A)]).await;
    let r = h.sync(&[A]).await;
    h.expect(A, "started").await;
    h.done(A, "write f1 one").await;
    h.done(A, "stall").await;
    h.stop(&r).await;
    h.expect(A, "stopped").await;
    assert_eq!(h.closed(&r).await, Some(RequestOutcome::Stopped));
    h.check_files(A);
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
    h.done(A, "write f2 two").await;
    h.done(A, "ok").await;
    assert_eq!(h.closed(&again).await, Some(RequestOutcome::Done));
    h.check_files(A);
    h.finish().await;
}

/// A step deaf to its stop is killed at the grace, and a write it had not
/// finished leaves nothing a reader could see.
#[tokio::test(flavor = "multi_thread")]
async fn a_step_deaf_to_its_stop_is_killed_and_leaves_only_what_it_acknowledged() {
    let mut h = Harness::new(&[step(A)]).await;
    let r = h.sync(&[A]).await;
    h.expect(A, "started").await;
    h.done(A, "on_stop ignore").await;
    h.done(A, "write kept yes").await;
    h.done(A, "spin").await;
    h.stop(&r).await;
    h.expect(A, "ignoring a stop").await;
    assert_eq!(h.closed(&r).await, Some(RequestOutcome::Stopped));
    h.until("a to be killed and settle", |rec, _| {
        rec.steps
            .get(A)?
            .last_run
            .as_ref()?
            .finished_at
            .as_ref()
            .map(|_| ())
    })
    .await;
    h.check_files(A);
    h.finish().await;
}

/// Pausing a running step stops it; resuming lets the next sync run it,
/// and the work it had not done happens then.
#[tokio::test(flavor = "multi_thread")]
async fn a_paused_step_stops_and_a_resumed_one_does_what_it_had_not() {
    let mut h = Harness::new(&[step(A)]).await;
    let r = h.sync(&[A]).await;
    h.expect(A, "started").await;
    h.done(A, "write before pause").await;
    h.done(A, "stall").await;
    h.pause(A).await;
    h.expect(A, "stopped").await;
    h.closed(&r).await;
    h.check_files(A);

    h.resume(A).await;
    let again = h.sync(&[A]).await;
    h.expect(A, "started").await;
    h.done(A, "write after resume").await;
    h.done(A, "ok").await;
    assert_eq!(h.closed(&again).await, Some(RequestOutcome::Done));
    h.check_files(A);
    h.finish().await;
}

/// A crash keeps every commit before it and nothing after the last one:
/// a batch applied but not committed is gone, and the next writer's open
/// is what throws it away.
#[tokio::test(flavor = "multi_thread")]
async fn a_crash_keeps_the_commits_before_it_and_loses_the_batch_after() {
    let mut h = Harness::new(&[step(A)]).await;
    let r = h.sync(&[A]).await;
    h.expect(A, "started").await;
    h.done(A, "batch commit put:a:1 put:b:2 put:c:3").await;
    h.done(A, "batch hold put:d:4 del:a").await;
    h.done(A, "crash").await;
    h.forget_held(A);
    h.closed(&r).await;
    h.check_rows(A).await;

    let again = h.sync(&[A]).await;
    h.expect(A, "started").await;
    h.done(A, "batch commit del:b put:c:30 put:e:5").await;
    h.done(A, "ok").await;
    assert_eq!(h.closed(&again).await, Some(RequestOutcome::Done));
    h.check_rows(A).await;
    h.finish().await;
}

/// A batch that changes nothing commits nothing: `main` does not move, so
/// nothing reading the store is told it did.
#[tokio::test(flavor = "multi_thread")]
async fn a_batch_that_changes_nothing_moves_nothing() {
    let mut h = Harness::new(&[step(A)]).await;
    let r = h.sync(&[A]).await;
    h.expect(A, "started").await;
    h.done(A, "batch commit put:a:1").await;
    let same = h.done(A, "batch commit put:a:1").await;
    assert_eq!(
        same, "committed -",
        "an upsert of what is there committed something"
    );
    h.done(A, "ok").await;
    h.closed(&r).await;
    h.check_rows(A).await;
    h.finish().await;
}

/// A consumer of a streaming producer runs on each seal, reads the store
/// at the version it was started against, and its queue rises with each
/// seal's rows and drains as it reads them.
#[tokio::test(flavor = "multi_thread")]
async fn a_consumer_reads_each_seal_at_its_version_and_its_queue_drains() {
    let mut h = Harness::new(&[step(A), reads(C, &[A])]).await;
    let r = h.sync(&[A]).await;
    h.expect(A, "started").await;
    h.done(A, "streams").await;
    h.done(A, "batch commit put:x:1 put:y:2").await;
    h.done(A, "seal v1 2").await;

    h.expect(C, "started").await;
    let queued = h
        .event("the consumer's queue to show the seal", |e| {
            matches!(e, Event::Metric { step, name, labels, value }
                if step == C && name == "queued" && labels.get("from").map(String::as_str) == Some(A) && *value == 2)
        })
        .await;
    drop(queued);
    assert_eq!(h.done(C, "count").await, format!("count {A}=2"));
    h.done(C, "ok").await;
    h.event("the queue to drain", |e| {
        matches!(e, Event::Metric { step, name, value, .. } if step == C && name == "queued" && *value == 0)
    })
    .await;

    h.done(A, "batch commit put:z:3 del:x").await;
    h.done(A, "seal v2 2").await;
    h.expect(C, "started").await;
    assert_eq!(h.done(C, "count").await, format!("count {A}=2"));
    h.done(C, "ok").await;
    h.done(A, "ok").await;
    assert_eq!(h.closed(&r).await, Some(RequestOutcome::Done));
    h.check_rows(A).await;
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
/// latest value, and a length and increments as done and queued.
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

/// A transient failure is retried in the same request, and the retry is
/// a new process that picks up the next instructions.
#[tokio::test(flavor = "multi_thread")]
async fn a_transient_failure_is_retried_by_a_new_process() {
    let mut h = Harness::new(&[step(A)]).await;
    let r = h.sync(&[A]).await;
    h.expect(A, "started 1").await;
    h.done(A, "write first try").await;
    h.done(A, "fail transient").await;
    h.expect(A, "started 2").await;
    h.done(A, "write second try").await;
    h.done(A, "ok").await;
    assert_eq!(h.closed(&r).await, Some(RequestOutcome::Done));
    h.check_files(A);
    h.finish().await;
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
    h.done(A, "write before transient").await;
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
    h.check_files(A);
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
