//! When a producer should seal what it has written so far.
//!
//! A step used to commit once, at the end. Committing in chunks lets a
//! consumer start on the early part while the rest is still arriving, and
//! makes a killed run keep what it had. This decides *when*; the commit
//! itself belongs to whoever owns the store.
//!
//! See `docs/dev/plans/streaming_steps_plan.md`.

use std::time::{Duration, Instant};

/// How often a producer seals what it has written: at most this long
/// between commits, asked at the producer's own consistent points.
///
/// There is no debounce half. A "seal once writes have been quiet" rule
/// cannot fire from a producer that only asks *as* it writes, and every
/// producer here does — so the dial existed and turned nothing. A burst that
/// ends is published by the next write past the ceiling, or by the run
/// finishing, which is the last seal.
///
/// The dial is the user's. How much latency to trade for how much `dolt_log`
/// is exactly the kind of call a person should get to make — what is *not*
/// theirs is whether a step can cope with chunked commits at all, which is a
/// fact about the step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cadence {
    pub at_most_every: Duration,
}

impl Default for Cadence {
    fn default() -> Self {
        Self {
            at_most_every: Duration::from_secs(15),
        }
    }
}

/// Asks "should I seal now?" — never fires on its own.
///
/// It has to be asked rather than tick, because only the caller knows where
/// its store is consistent. A commit landing mid-prune or mid-reconcile
/// publishes a store that is missing data it will have again a moment
/// later — and a provider that truncates and refills on every run
/// (`whatsapp`, `pdf`, `fsindex`) must not ask until the refill is done,
/// since half of one reads as a source that lost most of its data.
#[derive(Debug)]
pub struct Checkpointer {
    cadence: Cadence,
    last_commit: Instant,
    /// Rows written since the last commit. Zero means there is nothing to
    /// seal, and committing anyway would fill `dolt_log` with empty commits
    /// and wake every consumer to discover nothing moved.
    pending: u64,
}

impl Checkpointer {
    pub fn new(cadence: Cadence) -> Self {
        let now = Instant::now();
        Self {
            cadence,
            last_commit: now,
            pending: 0,
        }
    }

    /// Tell it work landed. Cheap enough to call per row.
    pub fn wrote(&mut self, rows: u64) {
        self.pending += rows;
    }

    /// Whether to seal now. Says yes at most once per batch of writes: the
    /// caller commits and calls [`Self::sealed`].
    pub fn should_seal(&self) -> bool {
        self.should_seal_at(Instant::now())
    }

    fn should_seal_at(&self, now: Instant) -> bool {
        if self.pending == 0 {
            return false;
        }
        now.duration_since(self.last_commit) >= self.cadence.at_most_every
    }

    /// Record that the caller committed.
    pub fn sealed(&mut self) {
        self.last_commit = Instant::now();
        self.pending = 0;
    }

    pub fn pending(&self) -> u64 {
        self.pending
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(c: &Checkpointer, after: Duration) -> bool {
        c.should_seal_at(c.last_commit + after)
    }

    #[test]
    fn a_writer_seals_once_the_ceiling_has_passed() {
        let mut c = Checkpointer::new(Cadence::default());
        c.wrote(1);
        assert!(!at(&c, Duration::from_secs(1)), "too soon");
        assert!(at(&c, Duration::from_secs(16)));
    }

    /// Nothing written means nothing to seal. Without this a long quiet
    /// stretch fills `dolt_log` with empty commits and wakes every consumer
    /// to discover nothing moved.
    #[test]
    fn nothing_written_never_seals() {
        let c = Checkpointer::new(Cadence::default());
        assert!(!at(&c, Duration::from_secs(600)));
    }

    #[test]
    fn sealing_clears_the_pending_work() {
        let mut c = Checkpointer::new(Cadence::default());
        c.wrote(5);
        assert_eq!(c.pending(), 5);
        c.sealed();
        assert_eq!(c.pending(), 0);
        assert!(!at(&c, Duration::from_secs(600)), "and does not seal again");
    }
}
