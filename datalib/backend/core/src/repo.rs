//! [`AppRepo`] — the seam to the two stores this server owns: filed
//! feedback and the sync job queue.

use std::sync::Arc;

use async_trait::async_trait;

use app_schema::disk_usage::DiskUsageRow;
use app_schema::feedback::FeedbackRow;
use app_schema::sync_jobs::SyncJobRow;

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("backend does not support write operations")]
    ReadOnly,
    #[error("internal: {0}")]
    Internal(String),
}

/// Writes and reads of the two application stores: filed feedback and
/// the sync job queue.
#[async_trait]
pub trait AppRepo: Send + Sync {
    /// Append a feedback row. The default impl returns
    /// [`RepoError::ReadOnly`]; only [`crate::dolt_repo::AppStore`]
    /// overrides it.
    async fn insert_feedback(&self, _row: FeedbackRow) -> Result<(), RepoError> {
        Err(RepoError::ReadOnly)
    }

    /// List `sync_jobs` rows. When `only_active` is true, returns only
    /// rows in `pending` or `running` state — used by the UI's polling
    /// chrome. Otherwise returns the most recent `limit` rows newest-first.
    /// Default impl returns [`RepoError::ReadOnly`].
    async fn list_jobs(
        &self,
        _only_active: bool,
        _limit: usize,
    ) -> Result<Vec<SyncJobRow>, RepoError> {
        Err(RepoError::ReadOnly)
    }

    /// Fetch a single sync job by id. Returns `Ok(None)` when not found.
    async fn get_job(&self, _job_id: &str) -> Result<Option<SyncJobRow>, RepoError> {
        Err(RepoError::ReadOnly)
    }

    /// Enqueue a new `pending` sync job. Implementations stamp the id
    /// (UUIDv4) and `created_at` themselves so callers don't have to.
    /// The new row is returned as written.
    async fn enqueue_job(
        &self,
        _kind: &str,
        _source_name: Option<&str>,
    ) -> Result<SyncJobRow, RepoError> {
        Err(RepoError::ReadOnly)
    }

    async fn request_cancel_job(&self, _job_id: &str) -> Result<(), RepoError> {
        Err(RepoError::ReadOnly)
    }

    // --- Worker-side job lifecycle ------------------------------------

    /// Atomically claim the oldest `pending` job: flip it to `running`,
    /// stamp `started_at`, and return the updated row. Returns `Ok(None)`
    /// when the queue is empty. Single-worker by construction, so the
    /// SELECT-then-UPDATE needs no extra locking beyond SQLite's
    /// single-writer guarantee.
    async fn claim_next_job(&self) -> Result<Option<SyncJobRow>, RepoError> {
        Err(RepoError::ReadOnly)
    }

    async fn set_job_pid(&self, _job_id: &str, _pid: i64) -> Result<(), RepoError> {
        Err(RepoError::ReadOnly)
    }

    /// Update the live progress fraction / message for a running job.
    /// Cheap, high-frequency write — deliberately does *not* mint a Dolt
    /// commit (only state transitions land in `dolt log`).
    async fn update_job_progress(
        &self,
        _job_id: &str,
        _pct: Option<f64>,
        _msg: Option<&str>,
    ) -> Result<(), RepoError> {
        Err(RepoError::ReadOnly)
    }

    async fn finish_job(
        &self,
        _job_id: &str,
        _state: &str,
        _error: Option<&str>,
    ) -> Result<(), RepoError> {
        Err(RepoError::ReadOnly)
    }

    async fn recover_running_jobs(&self) -> Result<usize, RepoError> {
        Err(RepoError::ReadOnly)
    }

    // --- The disk-usage timeseries ------------------------------------

    /// Append disk-usage samples. The caller has already applied the
    /// compaction rules (drop an unchanged value; never two samples for
    /// one series within five seconds) — this only writes.
    async fn record_disk_usage(&self, _rows: &[DiskUsageRow]) -> Result<(), RepoError> {
        Err(RepoError::ReadOnly)
    }

    async fn recent_disk_usage(&self, _limit: usize) -> Result<Vec<DiskUsageRow>, RepoError> {
        Err(RepoError::ReadOnly)
    }
}

/// Convenience alias for the dyn-dispatched app-store handle used by
/// HTTP handlers via `axum::State`.
pub type DynAppRepo = Arc<dyn AppRepo>;
