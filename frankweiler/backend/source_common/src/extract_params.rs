//! Cross-source extract give-up bounds.
//!
//! Lives in this schema-only foundation crate (depends on nothing but serde) so
//! both the shared HTTP retry chokepoint *below* the providers
//! (`frankweiler_etl::retry`) and the orchestrator config *above* them can name
//! it without either pulling the other's code. Relocated here from
//! `frankweiler_etl::extract_params` (which now re-exports it).

use serde::{Deserialize, Serialize};

/// Bounds on how hard a source's Extract step retries before the orchestrator
/// gives up on it. The shared HTTP chokepoint respects `Retry-After` on 429s
/// and otherwise backs off exponentially; these two knobs decide *when to
/// stop*. Both default when unset.
///
/// Settable globally (top-level `defaults.extract_params:`) and/or per-source
/// (`common.extract_params:`); the load-time `normalize()` folds the global
/// base into each source (source's `Some` fields win, `None` falls through),
/// and an unset field falls through to the built-in default.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtractParams {
    /// Give up on a source once this many minutes pass with no successful
    /// request. `None` → [`ExtractParams::DEFAULT_MAX_MINUTES_NO_PROGRESS`].
    #[serde(default)]
    pub maximum_time_without_progress_in_minutes: Option<u64>,
    /// Give up after this many consecutive retryable failures with no success
    /// in between. `None` → [`ExtractParams::DEFAULT_MAX_SEQUENTIAL_FAILURES`].
    #[serde(default)]
    pub maximum_sequential_failed_requests: Option<u64>,
}

impl ExtractParams {
    pub const DEFAULT_MAX_MINUTES_NO_PROGRESS: u64 = 30;
    pub const DEFAULT_MAX_SEQUENTIAL_FAILURES: u64 = 50;

    /// Merge `self` (a global base) with a per-source override. Source-level
    /// `Some(...)` wins; `None` falls through.
    pub fn merge(&self, source: &ExtractParams) -> ExtractParams {
        ExtractParams {
            maximum_time_without_progress_in_minutes: source
                .maximum_time_without_progress_in_minutes
                .or(self.maximum_time_without_progress_in_minutes),
            maximum_sequential_failed_requests: source
                .maximum_sequential_failed_requests
                .or(self.maximum_sequential_failed_requests),
        }
    }

    /// Resolved "max time without progress", applying the default.
    pub fn max_time_without_progress(&self) -> std::time::Duration {
        let mins = self
            .maximum_time_without_progress_in_minutes
            .unwrap_or(Self::DEFAULT_MAX_MINUTES_NO_PROGRESS);
        std::time::Duration::from_secs(mins.saturating_mul(60))
    }

    /// Resolved "max sequential failed requests", applying the default.
    pub fn max_sequential_failures(&self) -> u64 {
        self.maximum_sequential_failed_requests
            .unwrap_or(Self::DEFAULT_MAX_SEQUENTIAL_FAILURES)
    }
}
