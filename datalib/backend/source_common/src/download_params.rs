//! Cross-source download bounds: when to give up, and how many requests
//! may be out at once.

use serde::{Deserialize, Serialize};

/// Bounds on a source's download step. The shared HTTP chokepoint respects
/// `Retry-After` on 429s and otherwise backs off exponentially; the first
/// two knobs decide *when to stop*, the third how many requests the source
/// has out at once. All default when unset.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DownloadParams {
    /// Give up on a source once this many minutes pass with no successful
    /// request. Unset is 30.
    #[serde(default)]
    pub maximum_time_without_progress_in_minutes: Option<u64>,
    /// Give up after this many retryable failures in a row, with no success
    /// in between. Unset is 50.
    #[serde(default)]
    pub maximum_sequential_failed_requests: Option<u64>,
    /// Requests this source has out at once, however its download fans
    /// out. Each one is a `latchkey curl` process of its own, about 80 MB.
    /// Unset is 8.
    #[serde(default)]
    pub maximum_requests_at_once: Option<u64>,
}

impl DownloadParams {
    pub const DEFAULT_MAX_MINUTES_NO_PROGRESS: u64 = 30;
    pub const DEFAULT_MAX_SEQUENTIAL_FAILURES: u64 = 50;
    pub const DEFAULT_MAX_REQUESTS_AT_ONCE: u64 = 8;

    pub fn merge(&self, source: &DownloadParams) -> DownloadParams {
        DownloadParams {
            maximum_time_without_progress_in_minutes: source
                .maximum_time_without_progress_in_minutes
                .or(self.maximum_time_without_progress_in_minutes),
            maximum_sequential_failed_requests: source
                .maximum_sequential_failed_requests
                .or(self.maximum_sequential_failed_requests),
            maximum_requests_at_once: source
                .maximum_requests_at_once
                .or(self.maximum_requests_at_once),
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

    /// Never zero: a source that may send no request would hang.
    pub fn max_requests_at_once(&self) -> usize {
        let n = self
            .maximum_requests_at_once
            .unwrap_or(Self::DEFAULT_MAX_REQUESTS_AT_ONCE);
        usize::try_from(n).unwrap_or(usize::MAX).max(1)
    }
}
