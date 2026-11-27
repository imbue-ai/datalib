//! Catches a page feeding itself through the live stream: a request's
//! own effect — its log line, a walk it asked for — comes back as a
//! `root` frame, and the page refetches on the frame. Each hop is
//! counted, not prevented. The count rides the frame out (`chain`), comes
//! back on the refetch (`X-Datalib-Cause`), and is noted in [`Echoes`]
//! as the request's line is logged, where the watch takes it for the
//! frame that line causes. A chain that reaches [`LOOP_AT`] is a loop,
//! and the server says so in its log.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// The request header a page sends on a fetch it made while handling a
/// `root` frame: the frame's `chain`, or 0 when the frame had none.
pub const CAUSE_HEADER: &str = "x-datalib-cause";

/// The tracing target of the warning; `target:http.loop` in the log
/// panel's search bar.
pub const TARGET: &str = "http.loop";

/// One refetch caused by its own echo is waste; five in a row is a
/// page that will not stop by itself.
pub const LOOP_AT: u32 = 5;

tokio::task_local! {
    static CHAIN: u32;
}

/// The hops behind a request: what its cause header says, 0 for a
/// request nothing on the stream caused.
pub fn request_chain(header: Option<&str>) -> u32 {
    header.and_then(|v| v.trim().parse().ok()).unwrap_or(0)
}

/// Warn at [`LOOP_AT`] and at every tenfold after it, so a loop left
/// running says so again without a line per hop.
pub fn warns_at(chain: u32) -> bool {
    let mut at = LOOP_AT;
    loop {
        if chain == at {
            return true;
        }
        if chain < at {
            return false;
        }
        match at.checked_mul(10) {
            Some(next) => at = next,
            None => return false,
        }
    }
}

/// The chain the server's next log frame continues: one past the
/// longest chain among the request lines logged since the watch last
/// took it, or nothing when no request was logged, so no page caused the
/// frame. Kept in memory, because the server logged those lines itself;
/// reading them back from the run store is a read the store's own change
/// sets off again.
#[derive(Clone, Default)]
pub struct Echoes(Arc<AtomicU32>);

impl Echoes {
    /// A request's line was logged, `chain` hops from a fetch nothing caused.
    pub fn logged(&self, chain: u32) {
        // 0 is "nothing logged", so what is kept is the next chain.
        self.0.fetch_max(chain.saturating_add(1), Ordering::Relaxed);
    }

    /// The chain the frame for what was logged continues; resets it to 0.
    pub fn take(&self) -> Option<u32> {
        match self.0.swap(0, Ordering::Relaxed) {
            0 => None,
            next => Some(next),
        }
    }
}

/// Run a request's handler knowing its chain, so a frame the handler
/// sends itself (`usage::sample_on_demand`) continues it.
pub async fn scope<F: std::future::Future>(chain: u32, f: F) -> F::Output {
    CHAIN.scope(chain, f).await
}

/// The chain a frame sent from inside a request continues; `None`
/// outside one.
pub fn from_this_request() -> Option<u32> {
    CHAIN.try_with(|c| c.saturating_add(1)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_with_no_cause_starts_a_chain_at_zero() {
        assert_eq!(request_chain(None), 0);
        assert_eq!(request_chain(Some("3")), 3);
        assert_eq!(request_chain(Some("not a number")), 0);
    }

    #[test]
    fn the_requests_logged_continue_the_longest_chain_once() {
        let echoes = Echoes::default();
        assert_eq!(echoes.take(), None, "nothing was logged");
        echoes.logged(0);
        echoes.logged(4);
        echoes.logged(2);
        assert_eq!(echoes.take(), Some(5));
        assert_eq!(echoes.take(), None, "taken once");
        echoes.logged(0);
        assert_eq!(echoes.take(), Some(1), "a fetch nothing caused is one hop");
        echoes.logged(u32::MAX);
        assert_eq!(echoes.take(), Some(u32::MAX));
    }

    #[test]
    fn the_warning_comes_at_the_threshold_and_every_tenfold() {
        let warned: Vec<u32> = (0..6000).filter(|&c| warns_at(c)).collect();
        assert_eq!(warned, [5, 50, 500, 5000]);
        assert!(!warns_at(u32::MAX));
    }

    #[tokio::test]
    async fn a_frame_sent_inside_a_request_is_one_hop_past_it() {
        assert_eq!(from_this_request(), None);
        assert_eq!(scope(2, async { from_this_request() }).await, Some(3));
    }
}
