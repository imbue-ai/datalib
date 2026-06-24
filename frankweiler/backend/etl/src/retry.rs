//! Orchestrator-enforced give-up policy for the shared HTTP chokepoint.
//!
//! The mechanics of *waiting out* a rate limit live in
//! [`crate::http::latchkey_curl`]: it respects `Retry-After` on a 429 and
//! otherwise backs off exponentially. This module owns the orthogonal
//! question of *when to stop trying* — bounded by two knobs the
//! orchestrator resolves from config (`ExtractParams`) and installs once
//! per source:
//!
//!   - `maximum_sequential_failed_requests` — give up after this many
//!     consecutive retryable failures with no success in between.
//!   - `maximum_time_without_progress_in_minutes` — give up once this long
//!     passes with no successful request.
//!
//! It works exactly like [`crate::extract_metrics`]: a [`tokio::task_local`]
//! holds a [`RetryGuard`] for the duration of one source's extract
//! (installed by [`scope`]). The chokepoint resolves the ambient guard with
//! [`current_or_default`] and reports every attempt's outcome into it
//! ([`RetryGuard::on_progress`] / [`RetryGuard::on_failure`]). Providers
//! whose rate-limit signal the chokepoint can't see by status code (Slack's
//! HTTP-200 `error:"ratelimited"` body, GitHub's `403 + x-ratelimit-remaining:0`)
//! don't run their own loops — they hand the chokepoint a custom response
//! classifier (see [`crate::http::latchkey_curl_classified`]) so their quirk
//! flows through the *same* guard. The give-up policy thus lives in one
//! place rather than re-implemented per provider.
//!
//! Outside any `scope` (tests, standalone CLIs, the translate phase)
//! [`current_or_default`] hands back a fresh guard seeded from the built-in
//! defaults, so a single chokepoint call is still bounded and can never spin
//! forever.

use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::extract_params::ExtractParams;

/// Per-source give-up state, accumulated at the shared HTTP chokepoint for
/// the duration of one source's extract. Cheap to clone behind the `Arc`
/// the orchestrator hands out.
pub struct RetryGuard {
    /// Give up once `last_progress.elapsed()` reaches this.
    max_time_without_progress: Duration,
    /// Give up once `sequential_failures` reaches this.
    max_sequential_failures: u64,
    /// First backoff after a retryable failure with no `Retry-After`.
    initial_backoff: Duration,
    /// Ceiling the exponential backoff doubles up to.
    max_backoff: Duration,
    /// Consecutive retryable failures since the last success. Reset to 0 by
    /// [`RetryGuard::on_progress`].
    sequential_failures: AtomicU64,
    /// Instant of the last success (or guard creation). The
    /// time-without-progress budget is measured from here.
    last_progress: Mutex<Instant>,
}

/// What the guard says to do after a failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GuardVerdict {
    /// Keep going — neither give-up bound has been crossed.
    Continue,
    /// Stop; the string explains which bound tripped (for the error shown
    /// to the user).
    GiveUp(String),
}

impl RetryGuard {
    /// First backoff after a retryable failure that carried no `Retry-After`.
    pub const DEFAULT_INITIAL_BACKOFF: Duration = Duration::from_secs(2);
    /// Ceiling the exponential backoff doubles up to.
    pub const DEFAULT_MAX_BACKOFF: Duration = Duration::from_secs(60);

    /// Construct directly from resolved durations/counts. `from_params` is
    /// the normal path; this is exposed for tests that need sub-minute time
    /// budgets and near-zero backoff.
    pub fn new(
        max_time_without_progress: Duration,
        max_sequential_failures: u64,
        initial_backoff: Duration,
        max_backoff: Duration,
    ) -> Arc<Self> {
        Arc::new(Self {
            max_time_without_progress,
            max_sequential_failures,
            initial_backoff,
            max_backoff,
            sequential_failures: AtomicU64::new(0),
            last_progress: Mutex::new(Instant::now()),
        })
    }

    /// Build a guard from a source's resolved [`ExtractParams`], applying
    /// the built-in defaults for any unset field and the default backoff
    /// schedule.
    pub fn from_params(p: &ExtractParams) -> Arc<Self> {
        Self::new(
            p.max_time_without_progress(),
            p.max_sequential_failures(),
            Self::DEFAULT_INITIAL_BACKOFF,
            Self::DEFAULT_MAX_BACKOFF,
        )
    }

    pub fn initial_backoff(&self) -> Duration {
        self.initial_backoff
    }

    pub fn max_backoff(&self) -> Duration {
        self.max_backoff
    }

    /// Record a successful (or definitive) request: reset the failure
    /// streak and restart the no-progress clock.
    pub fn on_progress(&self) {
        self.sequential_failures.store(0, Ordering::Relaxed);
        *self.last_progress.lock().unwrap() = Instant::now();
    }

    /// Record a retryable failure and decide whether to keep going. Gives
    /// up when either the sequential-failure count or the
    /// time-without-progress budget is exhausted.
    pub fn on_failure(&self) -> GuardVerdict {
        let failures = self.sequential_failures.fetch_add(1, Ordering::Relaxed) + 1;
        if failures >= self.max_sequential_failures {
            return GuardVerdict::GiveUp(format!(
                "{failures} sequential failed requests (limit {})",
                self.max_sequential_failures
            ));
        }
        let elapsed = self.last_progress.lock().unwrap().elapsed();
        if elapsed >= self.max_time_without_progress {
            return GuardVerdict::GiveUp(format!(
                "no progress for {}s (limit {}s)",
                elapsed.as_secs(),
                self.max_time_without_progress.as_secs()
            ));
        }
        GuardVerdict::Continue
    }
}

tokio::task_local! {
    static GUARD: Arc<RetryGuard>;
}

/// Install `guard` as the ambient retry-guard context for the duration of
/// `fut`. The chokepoint (and report-in providers) invoked anywhere within
/// `fut` on the same task accumulate into it. Everything outside any
/// `scope` is a no-op.
pub async fn scope<F>(guard: Arc<RetryGuard>, fut: F) -> F::Output
where
    F: Future,
{
    GUARD.scope(guard, fut).await
}

/// The guard installed for the current source, or — outside any
/// [`scope`] — a fresh guard seeded from the built-in defaults that bounds
/// a single chokepoint call. Either way the caller gets something that
/// caps retries, so the chokepoint loop never spins forever.
pub fn current_or_default() -> Arc<RetryGuard> {
    GUARD
        .try_with(|g| g.clone())
        .unwrap_or_else(|_| RetryGuard::from_params(&ExtractParams::default()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FAST: Duration = Duration::from_millis(1);

    #[test]
    fn gives_up_after_sequential_failures() {
        // limit 3 → first two failures Continue, third gives up.
        let g = RetryGuard::new(Duration::from_secs(3600), 3, FAST, FAST);
        assert_eq!(g.on_failure(), GuardVerdict::Continue);
        assert_eq!(g.on_failure(), GuardVerdict::Continue);
        assert!(matches!(g.on_failure(), GuardVerdict::GiveUp(_)));
    }

    #[test]
    fn progress_resets_failure_streak() {
        let g = RetryGuard::new(Duration::from_secs(3600), 3, FAST, FAST);
        g.on_failure();
        g.on_failure();
        g.on_progress(); // streak back to 0
        assert_eq!(g.on_failure(), GuardVerdict::Continue);
        assert_eq!(g.on_failure(), GuardVerdict::Continue);
        assert!(matches!(g.on_failure(), GuardVerdict::GiveUp(_)));
    }

    #[test]
    fn gives_up_after_time_without_progress() {
        // Tiny time budget, generous failure count: the no-progress clock
        // is what trips.
        let g = RetryGuard::new(Duration::from_millis(10), 1_000_000, FAST, FAST);
        assert_eq!(g.on_failure(), GuardVerdict::Continue);
        std::thread::sleep(Duration::from_millis(20));
        assert!(matches!(g.on_failure(), GuardVerdict::GiveUp(_)));
    }

    #[tokio::test]
    async fn current_or_default_outside_scope_is_a_fresh_default_guard() {
        // No panic, and it carries the built-in default limits.
        let g = current_or_default();
        assert_eq!(
            g.max_sequential_failures,
            ExtractParams::DEFAULT_MAX_SEQUENTIAL_FAILURES
        );
    }

    #[tokio::test]
    async fn current_or_default_inside_scope_returns_the_installed_guard() {
        // limit 1 → the first failure on the installed guard gives up, and
        // `current_or_default` hands back that same shared guard.
        let g = RetryGuard::new(Duration::from_secs(3600), 1, FAST, FAST);
        let probe = g.clone();
        scope(g, async {
            let ambient = current_or_default();
            assert!(matches!(ambient.on_failure(), GuardVerdict::GiveUp(_)));
        })
        .await;
        // The shared state advanced through the task-local Arc.
        assert!(matches!(probe.on_failure(), GuardVerdict::GiveUp(_)));
    }
}
