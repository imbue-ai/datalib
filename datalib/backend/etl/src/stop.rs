//! The one bit a download reads between units of work: has this step been
//! asked to stop? Set once, by the SIGINT handler; read at every loop head
//! that starts a unit, by every seal decision, and under every backoff
//! sleep. A download that honours it ends at a boundary it chose and
//! `finish` commits there — the last seal — instead of being killed
//! mid-page with everything since the previous seal thrown away.
//!
//! A download can also stop itself, when the service it reads stopped
//! answering ([`StopFlag::give_up`]): the same boundary and the same
//! seal, but the step ends `incomplete`, not `cancelled`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

#[derive(Clone, Debug, Default)]
pub struct StopFlag(Arc<State>);

#[derive(Debug, Default)]
struct State {
    requested: AtomicBool,
    gave_up: OnceLock<String>,
}

/// How long a stopped sleep may still take to notice. A poll rather than a
/// wakeup so the flag needs nothing but an atomic; the step has seconds of
/// grace, and a backoff is the only wait long enough for this to matter.
const POLL: Duration = Duration::from_millis(200);

impl StopFlag {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn request(&self) {
        self.0.requested.store(true, Ordering::SeqCst);
    }

    pub fn requested(&self) -> bool {
        self.0.requested.load(Ordering::SeqCst)
    }

    /// Stop because going on is pointless, saying why. The first reason
    /// given is the one kept.
    pub fn give_up(&self, why: impl Into<String>) {
        let _ = self.0.gave_up.set(why.into());
        self.request();
    }

    /// Why the download stopped itself; `None` when it was asked to stop,
    /// or was not stopped.
    pub fn gave_up(&self) -> Option<&str> {
        self.0.gave_up.get().map(String::as_str)
    }

    /// Sleep for `d`, or until a stop is requested, whichever is first.
    /// `true` when it was the stop.
    pub async fn sleep_unless_stopped(&self, d: Duration) -> bool {
        let deadline = tokio::time::Instant::now() + d;
        loop {
            if self.requested() {
                return true;
            }
            let now = tokio::time::Instant::now();
            if now >= deadline {
                return false;
            }
            tokio::time::sleep((deadline - now).min(POLL)).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_stop_cuts_a_sleep_short() {
        let stop = StopFlag::new();
        let waiter = stop.clone();
        let handle =
            tokio::spawn(async move { waiter.sleep_unless_stopped(Duration::from_secs(60)).await });
        stop.request();
        assert!(handle.await.unwrap(), "the sleep should report the stop");
    }

    /// A give-up is a stop every loop already honours, and keeps the
    /// first reason, so the step can end `incomplete` saying why.
    #[test]
    fn a_give_up_is_a_stop_that_says_why() {
        let stop = StopFlag::new();
        let seen = stop.clone();
        stop.give_up("slack stopped answering");
        stop.give_up("a later reason");
        assert!(seen.requested());
        assert_eq!(seen.gave_up(), Some("slack stopped answering"));

        let asked = StopFlag::new();
        asked.request();
        assert_eq!(asked.gave_up(), None, "a stop asked for is not a give-up");
    }

    #[tokio::test]
    async fn an_unstopped_sleep_runs_to_its_end() {
        let stop = StopFlag::new();
        assert!(!stop.sleep_unless_stopped(Duration::from_millis(10)).await);
        assert!(!stop.requested());
    }
}
