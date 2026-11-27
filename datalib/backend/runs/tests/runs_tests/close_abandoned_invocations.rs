//! Closing the invocations of a loop that was killed. A loop closes each
//! invocation it sees end; one that was SIGKILLed ran no code to do it
//! with, and the next process to take the lock has to.

use datalib_runs::{close_abandoned_invocations, invocations_of, InvocationRow, Retention};

use crate::request::Request;

/// Whatever the caller calls an invocation nobody saw end. This crate
/// does not enumerate the scheduler's vocabulary, so the tests do not
/// either.
const STOPPED: &str = "stopped";

fn at(step: &str, state: &str) -> InvocationRow {
    InvocationRow {
        step: step.into(),
        state: state.into(),
        attempt: 1,
        updated_at_utc: crate::request::now_utc(),
        ..Default::default()
    }
}

/// A loop killed mid-request: two invocations still live, one done.
/// Dropping the writer leaves the live ones as they are, as a killed
/// loop does.
fn abandoned(root: &std::path::Path, started: &str) {
    let w = Request::dated(root, "r1", started, Retention::default());
    w.step(at("a/ingest", "running"));
    w.step(at("b/ingest", "pending"));
    w.step(at("c/ingest", "succeeded"));
}

async fn of(root: &std::path::Path, step: &str) -> InvocationRow {
    invocations_of(root, "r1")
        .await
        .unwrap()
        .into_iter()
        .find(|i| i.step == step)
        .unwrap_or_else(|| panic!("no {step}"))
}

/// Every invocation still live takes the state the caller named, with
/// the reason on it, so a person can tell one that stopped itself from
/// one the next loop gave up on. One that had already ended is left
/// exactly as it was.
#[tokio::test]
async fn it_closes_the_invocations_that_never_ended() {
    let td = tempfile::tempdir().unwrap();
    abandoned(td.path(), &crate::request::now_utc());
    assert_eq!(of(td.path(), "a/ingest").await.finished_at_utc, None);

    let closed = close_abandoned_invocations(td.path(), STOPPED, "the next loop closed it")
        .await
        .expect("close");
    assert_eq!(closed, 2, "the running one and the pending one");

    for step in ["a/ingest", "b/ingest"] {
        let i = of(td.path(), step).await;
        assert_eq!(i.state, STOPPED, "{step}");
        assert_eq!(i.error.as_deref(), Some("the next loop closed it"));
        assert!(i.finished_at_utc.is_some(), "{step}");
    }
    assert_eq!(
        of(td.path(), "c/ingest").await.state,
        "succeeded",
        "an invocation that ended is not rewritten"
    );
}

/// Calling it twice is calling it once: the second call finds nothing
/// live, and moves no stamp and no reason the first one set.
#[tokio::test]
async fn closing_twice_changes_nothing_the_second_time() {
    let td = tempfile::tempdir().unwrap();
    abandoned(td.path(), &crate::request::now_utc());
    assert_eq!(
        close_abandoned_invocations(td.path(), STOPPED, "first")
            .await
            .unwrap(),
        2
    );
    let stamp = of(td.path(), "a/ingest").await.finished_at_utc;
    assert_eq!(
        close_abandoned_invocations(td.path(), STOPPED, "second")
            .await
            .unwrap(),
        0
    );
    let a = of(td.path(), "a/ingest").await;
    assert_eq!(a.finished_at_utc, stamp);
    assert_eq!(a.error.as_deref(), Some("first"));
}

/// A root with no store at all is nothing to do rather than an error:
/// every process that takes the lock asks, a fresh root's first included.
#[tokio::test]
async fn a_root_with_no_store_is_not_an_error() {
    let td = tempfile::tempdir().unwrap();
    assert_eq!(
        close_abandoned_invocations(td.path(), STOPPED, "why")
            .await
            .expect("a missing store is fine"),
        0
    );
}

/// An invocation stamped ahead of the wall clock is closed no earlier
/// than it began, though the wall clock says it is earlier.
#[tokio::test]
async fn an_invocation_is_not_closed_before_it_began() {
    let td = tempfile::tempdir().unwrap();
    let ahead = "2369-04-15T00:00:00.000000+00:00";
    abandoned(td.path(), ahead);
    close_abandoned_invocations(td.path(), STOPPED, "gone")
        .await
        .expect("close");
    let a = of(td.path(), "a/ingest").await;
    assert_eq!(a.finished_at_utc.as_deref(), Some(ahead));
}
