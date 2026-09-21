//! The one bit a download reads between units of work: has this step been
//! asked to stop? Set once, by the SIGINT handler; read at every loop head
//! that starts a unit, by every seal decision, and under every backoff
//! sleep. A download that honours it ends at a boundary it chose and
//! `finish` commits there — the last seal — instead of being killed
//! mid-page with everything since the previous seal thrown away.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Debug, Default)]
pub struct StopFlag(Arc<AtomicBool>);

/// How long a stopped sleep may still take to notice. A poll rather than a
/// wakeup so the flag needs nothing but an atomic; the step has seconds of
/// grace, and a backoff is the only wait long enough for this to matter.
const POLL: Duration = Duration::from_millis(200);

impl StopFlag {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn request(&self) {
        self.0.store(true, Ordering::SeqCst);
    }

    pub fn requested(&self) -> bool {
        self.0.load(Ordering::SeqCst)
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

    #[tokio::test]
    async fn an_unstopped_sleep_runs_to_its_end() {
        let stop = StopFlag::new();
        assert!(!stop.sleep_unless_stopped(Duration::from_millis(10)).await);
        assert!(!stop.requested());
    }
}
