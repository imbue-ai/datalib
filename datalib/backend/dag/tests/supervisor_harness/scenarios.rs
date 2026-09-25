//! One small test per behaviour of the loop.

use std::time::Duration;

use datalib_dag::supervisor::store::RequestOutcome;
use datalib_dag::Event;

use crate::harness::{reads, source, Clocks, Harness, Seen, Step};

const SIGABRT: i32 = 6;
const SIGKILL: i32 = 9;

fn stops_soon() -> Clocks {
    Clocks {
        stop_grace: Duration::from_millis(50),
        ..Clocks::default()
    }
}

fn retries_at_once() -> Clocks {
    Clocks {
        backoff: Duration::ZERO,
        ..Clocks::default()
    }
}

/// A `queued` metric the loop keeps on `consumer` for what `producer` has
/// sealed and it has not read.
fn queued(e: &Seen, consumer: &str, producer: &str) -> Option<i64> {
    match e {
        Seen::Event(Event::Metric {
            step,
            name,
            labels,
            value,
        }) if step == consumer
            && name == "queued"
            && labels.get("from").map(String::as_str) == Some(producer) =>
        {
            Some(*value)
        }
        _ => None,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_sync_runs_its_source_once_and_closes_done() {
    let mut h = Harness::new(&[source("a")]).await;
    let sync = h.sync(&["a"]).await;
    h.started("a").await;
    h.run("a", "ok v1").await;
    assert_eq!(h.closed(&sync).await, RequestOutcome::Done);
    let end = h.ended("a", 1).await;
    assert_eq!(end.outcome, "succeeded");
    assert_eq!(end.exit_code, Some(0));
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stop_mid_stall_ends_the_process_and_the_request() {
    let mut h = Harness::new(&[source("a")]).await;
    let sync = h.sync(&["a"]).await;
    h.started("a").await;
    h.run("a", "stall").await;
    h.stop(&sync).await;
    h.ack("a", "sigint").await;
    assert_eq!(h.closed(&sync).await, RequestOutcome::Stopped);
    let end = h.ended("a", 1).await;
    assert_eq!(
        (end.outcome.as_str(), end.exit_code),
        ("stopped", Some(130))
    );
    h.finish().await;
}

/// Stopping, then Stopped: the row reads Running with a `stopping`
/// detail for as long as the process lives after its SIGINT.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_reads_stopping_until_the_process_ends() {
    let mut h = Harness::new(&[source("a")]).await;
    let sync = h.sync(&["a"]).await;
    h.started("a").await;
    // Deaf, but still reading instructions: it outlives its SIGINT until
    // told to end.
    h.run("a", "on_stop ignore").await;
    h.stop(&sync).await;
    h.ack("a", "sigint").await;
    h.until("a to read Stopping", |s| {
        let st = s.record.steps.get("a")?;
        let stopping = st.state_detail.as_deref()?.starts_with("stopping");
        (stopping && st.state.map(|k| k.as_str()) == Some("running")).then_some(())
    })
    .await;
    assert_eq!(h.closed(&sync).await, RequestOutcome::Stopped);
    h.run("a", "fail cancelled").await;
    let end = h.ended("a", 1).await;
    assert_eq!(end.outcome, "stopped", "{end:?}");
    h.until("a to stop reading Stopping", |s| {
        let st = s.record.steps.get("a")?;
        (st.state.map(|k| k.as_str()) != Some("running")).then_some(())
    })
    .await;
    h.finish().await;
}

/// A step deaf to its SIGINT is killed once the stop grace is up.
#[tokio::test(flavor = "multi_thread")]
async fn a_step_deaf_to_its_stop_is_killed_at_the_grace() {
    let mut h = Harness::with(&[source("a")], stops_soon()).await;
    let sync = h.sync(&["a"]).await;
    h.started("a").await;
    h.run("a", "on_stop ignore").await;
    h.run("a", "stall").await;
    h.stop(&sync).await;
    h.ack("a", "sigint").await;
    let end = h.ended("a", 1).await;
    assert_eq!(
        (end.outcome.as_str(), end.signal),
        ("stopped", Some(SIGKILL))
    );
    assert_eq!(h.closed(&sync).await, RequestOutcome::Stopped);
    h.finish().await;
}

/// A pause stops a running step; a resume while its request is still
/// open runs it again.
#[tokio::test(flavor = "multi_thread")]
async fn a_paused_step_stops_and_runs_again_on_resume() {
    let mut h = Harness::new(&[source("a"), source("b")]).await;
    let sync = h.sync(&["a", "b"]).await;
    h.started("a").await;
    h.started("b").await;
    h.run("a", "stall").await;
    h.pause("a").await;
    h.ack("a", "sigint").await;
    assert_eq!(h.ended("a", 1).await.outcome, "stopped");
    // b holds the request open.
    h.resume("a").await;
    h.started("a").await;
    h.run("a", "ok v1").await;
    h.run("b", "ok v1").await;
    assert_eq!(h.closed(&sync).await, RequestOutcome::Done);
    assert_eq!(h.ended("a", 2).await.outcome, "succeeded");
    h.finish().await;
}

/// A stop during a retry's backoff ends the wait at once: the backoff
/// here is an hour.
#[tokio::test(flavor = "multi_thread")]
async fn a_stop_during_a_retry_backoff_ends_it_at_once() {
    let mut h = Harness::new(&[source("a")]).await;
    let sync = h.sync(&["a"]).await;
    h.started("a").await;
    h.run("a", "fail transient").await;
    h.stop(&sync).await;
    assert_eq!(h.closed(&sync).await, RequestOutcome::Stopped);
    let end = h.ended("a", 1).await;
    assert_eq!((end.outcome.as_str(), end.attempts), ("stopped", 1));
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_pause_during_a_retry_backoff_ends_it_at_once() {
    let mut h = Harness::new(&[source("a")]).await;
    let sync = h.sync(&["a"]).await;
    h.started("a").await;
    h.run("a", "fail transient").await;
    h.pause("a").await;
    let end = h.ended("a", 1).await;
    assert_eq!((end.outcome.as_str(), end.attempts), ("stopped", 1));
    assert_eq!(h.closed(&sync).await, RequestOutcome::Done);
    h.finish().await;
}

/// A transient failure is retried inside the same invocation, by a new
/// process.
#[tokio::test(flavor = "multi_thread")]
async fn a_transient_failure_is_retried_by_a_new_process_in_one_invocation() {
    let mut h = Harness::with(&[source("a")], retries_at_once()).await;
    let sync = h.sync(&["a"]).await;
    let first = h.started("a").await;
    h.run("a", "fail transient").await;
    let second = h.ack("a", "started 2").await;
    assert_ne!(first, second);
    h.run("a", "ok v1").await;
    assert_eq!(h.closed(&sync).await, RequestOutcome::Done);
    let end = h.ended("a", 1).await;
    assert_eq!((end.outcome.as_str(), end.attempts), ("succeeded", 2));
    assert_eq!(h.state().await.started("a"), 1, "one invocation");
    h.finish().await;
}

/// Syncs made while a step runs cost exactly one more run between them.
#[tokio::test(flavor = "multi_thread")]
async fn rapid_syncs_while_running_cost_exactly_one_more_run() {
    let mut h = Harness::new(&[source("a")]).await;
    let first = h.sync(&["a"]).await;
    h.started("a").await;
    let more = [h.sync(&["a"]).await, h.sync(&["a"]).await];
    h.run("a", "ok v1").await;
    h.started("a").await;
    h.run("a", "ok v1").await;
    for id in [&first, &more[0], &more[1]] {
        assert_eq!(h.closed(id).await, RequestOutcome::Done);
    }
    assert_eq!(h.state().await.started("a"), 2);
    h.finish().await;
}

/// A burst of syncs on an idle loop runs the step one at a time — the
/// harness checks that on every start — and never more often than asked.
#[tokio::test(flavor = "multi_thread")]
async fn a_burst_of_syncs_while_idle_never_overlaps_or_runs_more_than_asked() {
    let mut h = Harness::new(&[source("a")]).await;
    for _ in 0..5 {
        h.tell("a", "ok v1");
    }
    let mut syncs = Vec::new();
    for _ in 0..5 {
        syncs.push(h.sync(&["a"]).await);
    }
    for id in &syncs {
        assert_eq!(h.closed(id).await, RequestOutcome::Done);
    }
    let runs = h.state().await.started("a");
    assert!((1..=5).contains(&runs), "{runs} runs for 5 syncs");
    h.finish().await;
}

/// A source added to the config mid-sync starts beside the one running.
#[tokio::test(flavor = "multi_thread")]
async fn a_source_added_mid_sync_starts_beside_the_running_one() {
    let mut h = Harness::new(&[source("a")]).await;
    let first = h.sync(&["a"]).await;
    h.started("a").await;
    h.edit_config(&[source("a"), source("b")]);
    let second = h.sync(&["b"]).await;
    h.started("b").await;
    assert_eq!(h.state().await.outcome(&first), None, "a still runs");
    h.run("b", "ok v1").await;
    h.run("a", "ok v1").await;
    assert_eq!(h.closed(&first).await, RequestOutcome::Done);
    assert_eq!(h.closed(&second).await, RequestOutcome::Done);
    h.finish().await;
}

/// How each process ended is what the record says: an exit code, an
/// abort, a SIGKILL. None of them wrote an outcome, so none is retried.
#[tokio::test(flavor = "multi_thread")]
async fn each_way_a_process_ends_is_recorded() {
    let mut h = Harness::new(&[source("a"), source("b"), source("c")]).await;
    let sync = h.sync(&["a", "b", "c"]).await;
    for step in ["a", "b", "c"] {
        h.started(step).await;
    }
    h.run("a", "exit 3").await;
    h.run("b", "crash").await;
    h.run("c", "kill").await;
    assert_eq!(h.closed(&sync).await, RequestOutcome::Failed);
    let a = h.ended("a", 1).await;
    let b = h.ended("b", 1).await;
    let c = h.ended("c", 1).await;
    assert_eq!((a.outcome.as_str(), a.exit_code), ("failed", Some(3)));
    assert_eq!((b.outcome.as_str(), b.signal), ("failed", Some(SIGABRT)));
    assert_eq!((c.outcome.as_str(), c.signal), ("failed", Some(SIGKILL)));
    for end in [&a, &b, &c] {
        assert_eq!(end.attempts, 1, "{end:?}");
    }
    h.finish().await;
}

fn chain() -> Vec<Step> {
    vec![source("a"), reads("c", &["a"])]
}

/// A consumer runs when what it reads moves, and not when it does not;
/// it is handed the version the loop recorded.
#[tokio::test(flavor = "multi_thread")]
async fn a_consumer_runs_only_when_its_producers_version_moves() {
    let mut h = Harness::new(&chain()).await;
    let sync = h.sync(&["a"]).await;
    h.started("a").await;
    h.run("a", "ok v1").await;
    h.started("c").await;
    h.run("c", "ok c1").await;
    assert_eq!(h.closed(&sync).await, RequestOutcome::Done);

    let again = h.sync(&["a"]).await;
    h.started("a").await;
    h.run("a", "ok v1").await;
    assert_eq!(h.closed(&again).await, RequestOutcome::Done);
    assert_eq!(h.state().await.started("c"), 1, "nothing moved for c");

    let moved = h.sync(&["a"]).await;
    h.started("a").await;
    h.run("a", "ok v2").await;
    h.started("c").await;
    h.run("c", "ok c2").await;
    assert_eq!(h.closed(&moved).await, RequestOutcome::Done);
    h.finish().await;
}

/// The consumer is handed exactly what the loop recorded for its producer.
#[tokio::test(flavor = "multi_thread")]
async fn a_consumer_is_handed_the_version_the_loop_recorded() {
    let mut h = Harness::new(&chain()).await;
    let sync = h.sync(&["a"]).await;
    h.started("a").await;
    h.run("a", "ok v1").await;
    h.started("c").await;
    h.tell("c", "reads");
    let reads = h
        .wait("c's reads", |s| match s {
            Seen::Ack { step, what, .. } if step == "c" && what.starts_with("reads ") => {
                Some(what["reads ".len()..].to_string())
            }
            _ => None,
        })
        .await;
    let recorded = h
        .until("a's version in the record", |s| {
            s.version("a").map(str::to_string)
        })
        .await;
    let handed: std::collections::BTreeMap<String, String> = serde_json::from_str(&reads).unwrap();
    assert_eq!(handed.get("a"), Some(&recorded));
    assert!(recorded.ends_with(":v1"), "{recorded}");
    h.run("c", "ok c1").await;
    assert_eq!(h.closed(&sync).await, RequestOutcome::Done);
    h.finish().await;
}

/// A consumer runs on each seal of a streaming producer, one pass at a
/// time, and its queue rises on seals and drains on reads.
#[tokio::test(flavor = "multi_thread")]
async fn a_consumer_runs_on_each_seal_of_a_streaming_producer() {
    let mut h = Harness::new(&chain()).await;
    let sync = h.sync(&["a"]).await;
    h.started("a").await;
    h.run("a", "streams").await;
    h.run("a", "seal s1 5").await;
    h.wait("c's queue to rise to 5", |s| {
        queued(s, "c", "a").filter(|v| *v == 5)
    })
    .await;
    h.started("c").await;
    h.run("c", "ok c1").await;
    h.wait("c's queue to drain", |s| {
        queued(s, "c", "a").filter(|v| *v == 0)
    })
    .await;
    h.run("a", "seal s2 3").await;
    h.started("c").await;
    h.run("c", "ok c2").await;
    h.run("a", "ok s3").await;
    h.started("c").await;
    h.run("c", "ok c3").await;
    assert_eq!(h.closed(&sync).await, RequestOutcome::Done);
    assert_eq!(h.state().await.started("c"), 3);
    h.finish().await;
}

/// A producer that does not stream holds its consumer back even when it
/// seals: what it wrote may be half-done.
#[tokio::test(flavor = "multi_thread")]
async fn a_producer_that_does_not_stream_holds_its_consumer_until_it_ends() {
    let mut h = Harness::new(&chain()).await;
    let sync = h.sync(&["a"]).await;
    h.started("a").await;
    h.run("a", "seal s1").await;
    h.until("c to wait for a after the seal", |s| {
        let moved = s.version("a").is_some_and(|v| v.contains("s1"));
        (moved && s.detail("c") == Some("waiting for a")).then_some(())
    })
    .await;
    assert_eq!(h.state().await.started("c"), 0);
    h.run("a", "ok v1").await;
    h.started("c").await;
    h.run("c", "ok c1").await;
    assert_eq!(h.closed(&sync).await, RequestOutcome::Done);
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_a_streaming_producers_request_stops_its_consumer() {
    let mut h = Harness::new(&chain()).await;
    let sync = h.sync(&["a"]).await;
    h.started("a").await;
    h.run("a", "streams").await;
    h.run("a", "seal s1").await;
    h.started("c").await;
    h.run("c", "stall").await;
    h.stop(&sync).await;
    h.ack("a", "sigint").await;
    h.ack("c", "sigint").await;
    assert_eq!(h.closed(&sync).await, RequestOutcome::Stopped);
    assert_eq!(h.ended("a", 1).await.outcome, "stopped");
    assert_eq!(h.ended("c", 1).await.outcome, "stopped");
    h.finish().await;
}

/// Pausing the producer stops it; the consumer mid-pass finishes.
#[tokio::test(flavor = "multi_thread")]
async fn pausing_a_streaming_producer_lets_its_consumer_finish() {
    let mut h = Harness::new(&chain()).await;
    let sync = h.sync(&["a"]).await;
    h.started("a").await;
    h.run("a", "streams").await;
    h.run("a", "seal s1").await;
    h.started("c").await;
    h.pause("a").await;
    h.ack("a", "sigint").await;
    h.run("c", "ok c1").await;
    assert_eq!(h.ended("c", 1).await.outcome, "succeeded");
    assert_eq!(h.closed(&sync).await, RequestOutcome::Done);
    h.finish().await;
}

/// A paused consumer waits out its producer's seals and runs once, on
/// the newest, when resumed.
#[tokio::test(flavor = "multi_thread")]
async fn a_paused_consumer_waits_out_seals_and_runs_on_resume() {
    let mut h = Harness::new(&chain()).await;
    h.pause("c").await;
    let sync = h.sync(&["a"]).await;
    h.started("a").await;
    h.run("a", "streams").await;
    h.run("a", "seal s1").await;
    h.run("a", "seal s2").await;
    h.until("c to read paused after the seals", |s| {
        let sealed = s.version("a").is_some_and(|v| v.contains("s2"));
        (sealed && s.detail("c") == Some("paused by person")).then_some(())
    })
    .await;
    assert_eq!(h.state().await.started("c"), 0);
    h.resume("c").await;
    h.started("c").await;
    h.tell("c", "reads");
    let reads = h
        .wait("c's reads", |s| match s {
            Seen::Ack { step, what, .. } if step == "c" && what.starts_with("reads ") => {
                Some(what.clone())
            }
            _ => None,
        })
        .await;
    assert!(reads.contains("s2"), "{reads}");
    h.run("c", "ok c1").await;
    h.run("a", "ok s3").await;
    h.started("c").await;
    h.run("c", "ok c2").await;
    assert_eq!(h.closed(&sync).await, RequestOutcome::Done);
    h.finish().await;
}

/// Ctrl-C: the host going stops every step it started, and leaves the
/// requests open for the next loop.
#[tokio::test(flavor = "multi_thread")]
async fn stopping_the_host_stops_every_step_and_leaves_requests_open() {
    let mut h = Harness::new(&[source("a"), source("b")]).await;
    let sync = h.sync(&["a", "b"]).await;
    h.started("a").await;
    h.started("b").await;
    h.run("a", "stall").await;
    h.stop_host().await;
    h.ack("a", "sigint").await;
    h.ack("b", "sigint").await;
    assert_eq!(
        h.state().await.outcome(&sync),
        None,
        "left for the next loop"
    );
    h.finish().await;
}

// Graphs: all the parallelism the rules allow, and no more.

/// Fan-out: every consumer of a streaming producer starts on its seal,
/// all at once.
#[tokio::test(flavor = "multi_thread")]
async fn a_fan_out_starts_every_consumer_on_one_seal() {
    let steps = [
        source("a"),
        reads("c1", &["a"]),
        reads("c2", &["a"]),
        reads("c3", &["a"]),
    ];
    let mut h = Harness::new(&steps).await;
    let sync = h.sync(&["a"]).await;
    h.started("a").await;
    h.run("a", "streams").await;
    h.run("a", "seal s1").await;
    // All three are started before any is told to end.
    for c in ["c1", "c2", "c3"] {
        h.started(c).await;
    }
    for c in ["c1", "c2", "c3"] {
        h.run(c, "ok v1").await;
    }
    h.run("a", "ok s1").await;
    assert_eq!(h.closed(&sync).await, RequestOutcome::Done);
    h.finish().await;
}

/// Fan-in: a consumer of two streaming sources starts on the first one's
/// seal without waiting for the other, and runs again for the second.
#[tokio::test(flavor = "multi_thread")]
async fn a_fan_in_does_not_wait_for_its_slowest_source() {
    let steps = [source("a"), source("b"), reads("d", &["a", "b"])];
    let mut h = Harness::new(&steps).await;
    let sync = h.sync(&["a", "b"]).await;
    h.started("a").await;
    h.started("b").await;
    h.run("a", "streams").await;
    h.run("b", "streams").await;
    h.run("a", "seal a1").await;
    h.started("d").await;
    h.run("b", "seal b1").await;
    h.run("d", "ok d1").await;
    h.started("d").await;
    h.run("d", "ok d2").await;
    h.run("a", "ok a1").await;
    h.run("b", "ok b1").await;
    assert_eq!(h.closed(&sync).await, RequestOutcome::Done);
    assert_eq!(h.state().await.started("d"), 2);
    h.finish().await;
}

/// A streaming chain: each hop starts on the seal of the one before, so
/// all three run at once.
#[tokio::test(flavor = "multi_thread")]
async fn a_streaming_chain_runs_every_hop_at_once() {
    let steps = [source("a"), reads("c", &["a"]), reads("e", &["c"])];
    let mut h = Harness::new(&steps).await;
    let sync = h.sync(&["a"]).await;
    h.started("a").await;
    h.run("a", "streams").await;
    h.run("a", "seal a1").await;
    h.started("c").await;
    h.run("c", "streams").await;
    h.run("c", "seal c1").await;
    h.started("e").await;
    // a, c and e are all running now.
    h.run("e", "ok e1").await;
    h.run("c", "ok c1").await;
    h.run("a", "ok a1").await;
    assert_eq!(h.closed(&sync).await, RequestOutcome::Done);
    h.finish().await;
}

// A fan-in under pressure: wake-ups in bursts, mixed with stops and pauses.

/// The versions `step` was handed, input → version, from its `reads` ack.
async fn handed(h: &mut Harness, step: &str) -> std::collections::BTreeMap<String, String> {
    h.tell(step, "reads");
    let json = h
        .wait("the versions it was handed", |s| match s {
            Seen::Ack { step: st, what, .. } if st == step && what.starts_with("reads ") => {
                Some(what["reads ".len()..].to_string())
            }
            _ => None,
        })
        .await;
    serde_json::from_str(&json).unwrap()
}

fn fan_in(sources: &[&str]) -> Vec<Step> {
    let mut steps: Vec<Step> = sources.iter().map(|s| source(s)).collect();
    steps.push(reads("d", sources));
    steps
}

/// Seals from three sources arriving while the fan-in runs, in a burst,
/// cost it exactly one more pass, which reads the newest of each.
#[tokio::test(flavor = "multi_thread")]
async fn a_burst_of_seals_costs_a_running_fan_in_exactly_one_more_pass_on_the_newest() {
    let mut h = Harness::new(&fan_in(&["a", "b", "c"])).await;
    let sync = h.sync(&["a", "b", "c"]).await;
    for s in ["a", "b", "c"] {
        h.started(s).await;
        h.run(s, "streams").await;
    }
    h.run("a", "seal a1").await;
    h.started("d").await;
    for (s, v) in [("b", "b1"), ("c", "c1"), ("a", "a2"), ("b", "b2")] {
        h.run(s, &format!("seal {v}")).await;
    }
    h.run("d", "ok d1").await;
    h.started("d").await;
    let read = handed(&mut h, "d").await;
    for (input, v) in [("a", ":a2"), ("b", ":b2"), ("c", ":c1")] {
        assert!(read[input].ends_with(v), "{input}: {read:?}");
    }
    h.run("d", "ok d2").await;
    // Each source ends on the version it last sealed: nothing moves.
    for (s, v) in [("a", "a2"), ("b", "b2"), ("c", "c1")] {
        h.run(s, &format!("ok {v}")).await;
    }
    assert_eq!(h.closed(&sync).await, RequestOutcome::Done);
    assert_eq!(h.state().await.started("d"), 2);
    h.finish().await;
}

/// A fan-in serving two syncs outlives the stop of one: the other still
/// wants it, and it runs again when that one's source seals.
#[tokio::test(flavor = "multi_thread")]
async fn stopping_one_of_two_syncs_a_fan_in_serves_leaves_it_to_the_other() {
    let mut h = Harness::new(&fan_in(&["a", "b"])).await;
    let sync_a = h.sync(&["a"]).await;
    let sync_b = h.sync(&["b"]).await;
    for s in ["a", "b"] {
        h.started(s).await;
        h.run(s, "streams").await;
    }
    h.run("a", "seal a1").await;
    h.started("d").await;
    h.stop(&sync_a).await;
    h.ack("a", "sigint").await;
    assert_eq!(h.closed(&sync_a).await, RequestOutcome::Stopped);
    // Not stopped: it takes an instruction and ends on its own.
    h.run("d", "ok d1").await;
    assert_eq!(h.ended("d", 1).await.outcome, "succeeded");
    h.run("b", "seal b1").await;
    h.started("d").await;
    h.run("d", "ok d2").await;
    h.run("b", "ok b1").await;
    assert_eq!(h.closed(&sync_b).await, RequestOutcome::Done);
    h.finish().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stopping_every_sync_a_fan_in_serves_stops_it() {
    let mut h = Harness::new(&fan_in(&["a", "b"])).await;
    let syncs = [h.sync(&["a"]).await, h.sync(&["b"]).await];
    for s in ["a", "b"] {
        h.started(s).await;
        h.run(s, "streams").await;
    }
    h.run("a", "seal a1").await;
    h.started("d").await;
    h.run("d", "stall").await;
    for id in &syncs {
        h.stop(id).await;
    }
    h.ack("d", "sigint").await;
    assert_eq!(h.ended("d", 1).await.outcome, "stopped");
    for id in &syncs {
        assert_eq!(h.closed(id).await, RequestOutcome::Stopped);
    }
    h.finish().await;
}

/// A paused fan-in waits out a burst of seals and runs once on resume,
/// on the newest of each input.
#[tokio::test(flavor = "multi_thread")]
async fn a_paused_fan_in_waits_out_a_burst_and_runs_once_on_resume() {
    let mut h = Harness::new(&fan_in(&["a", "b"])).await;
    h.pause("d").await;
    let sync = h.sync(&["a", "b"]).await;
    for s in ["a", "b"] {
        h.started(s).await;
        h.run(s, "streams").await;
    }
    for (s, v) in [("a", "a1"), ("b", "b1"), ("a", "a2"), ("b", "b2")] {
        h.run(s, &format!("seal {v}")).await;
    }
    h.until("d to read paused after the burst", |s| {
        let sealed = s.version("a").is_some_and(|v| v.ends_with(":a2"))
            && s.version("b").is_some_and(|v| v.ends_with(":b2"));
        (sealed && s.detail("d") == Some("paused by person")).then_some(())
    })
    .await;
    assert_eq!(h.state().await.started("d"), 0);
    h.resume("d").await;
    h.started("d").await;
    let read = handed(&mut h, "d").await;
    assert!(
        read["a"].ends_with(":a2") && read["b"].ends_with(":b2"),
        "{read:?}"
    );
    h.run("d", "ok d1").await;
    h.run("a", "ok a2").await;
    h.run("b", "ok b2").await;
    assert_eq!(h.closed(&sync).await, RequestOutcome::Done);
    assert_eq!(h.state().await.started("d"), 1);
    h.finish().await;
}

/// A step that says it failed is not a stopped run, though the loop had
/// just asked it to stop: its failure stands, and a resume does not run it
/// again for the request it failed. (One that exits with no word after a
/// stop was asked is taken to have stopped: that is all a plain command
/// can tell us.)
#[tokio::test(flavor = "multi_thread")]
async fn a_step_that_fails_after_being_asked_to_stop_is_a_failure_not_a_stop() {
    // `b` holds the request open across the pause and the resume.
    let mut h = Harness::new(&[source("a"), source("b")]).await;
    let sync = h.sync(&["a", "b"]).await;
    h.started("a").await;
    h.started("b").await;
    h.run("a", "on_stop ignore").await;
    h.pause("a").await;
    h.ack("a", "sigint").await;
    h.run("a", "fail data").await;
    assert_eq!(h.ended("a", 1).await.outcome, "failed");
    h.resume("a").await;
    h.run("b", "ok v1").await;
    h.until("the request to close, or a to run again", |s| {
        (s.outcome(&sync).is_some() || s.started("a") > 1).then_some(())
    })
    .await;
    assert_eq!(
        h.state().await.started("a"),
        1,
        "a failure is not run again"
    );
    assert_eq!(h.closed(&sync).await, RequestOutcome::Failed);
    h.finish().await;
}
