//! Cross-source download give-up bounds.

use serde::{Deserialize, Serialize};

/// Bounds on how hard a source's download step retries before the orchestrator
/// gives up on it. The shared HTTP chokepoint respects `Retry-After` on 429s
/// and otherwise backs off exponentially; these two knobs decide *when to
/// stop*. Both default when unset.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadParams {
    /// Give up on a source once this many minutes pass with no successful
    /// request. `None` → [`DownloadParams::DEFAULT_MAX_MINUTES_NO_PROGRESS`].
    #[serde(default)]
    pub maximum_time_without_progress_in_minutes: Option<u64>,
    /// Give up after this many consecutive retryable failures with no success
    /// in between. `None` → [`DownloadParams::DEFAULT_MAX_SEQUENTIAL_FAILURES`].
    #[serde(default)]
    pub maximum_sequential_failed_requests: Option<u64>,
}

impl DownloadParams {
    pub const DEFAULT_MAX_MINUTES_NO_PROGRESS: u64 = 30;
    pub const DEFAULT_MAX_SEQUENTIAL_FAILURES: u64 = 50;

    pub fn merge(&self, source: &DownloadParams) -> DownloadParams {
        DownloadParams {
            maximum_time_without_progress_in_minutes: source
                .maximum_time_without_progress_in_minutes
                .or(self.maximum_time_without_progress_in_minutes),
            maximum_sequential_failed_requests: source
                .maximum_sequential_failed_requests
                .or(self.maximum_sequential_failed_requests),
        }
    }

    pub fn max_time_without_progress(&self) -> std::time::Duration {
        let mins = self
            .maximum_time_without_progress_in_minutes
            .unwrap_or(Self::DEFAULT_MAX_MINUTES_NO_PROGRESS);
        std::time::Duration::from_secs(mins.saturating_mul(60))
    }

    pub fn max_sequential_failures(&self) -> u64 {
        self.maximum_sequential_failed_requests
            .unwrap_or(Self::DEFAULT_MAX_SEQUENTIAL_FAILURES)
    }
}
