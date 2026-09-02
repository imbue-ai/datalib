//! [`AppRepo`] — the seam to the two stores this server owns: filed
//! feedback and the sync job queue.
//!
//! The grid index is not here. It is read through
//! `datalib_unified_index::repo::IndexRepo`, in the crate the applet
//! links, because it is a different file with a different writer.
//! [`RepoError`] stays shared: both seams report failures the same way.

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
///
/// Separate from [`IndexRepo`] because they are separate files with a
/// different writer. One process owns both; the index is owned by the
/// pipeline.
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

    /// Request cancellation of a pending/running job. Flips `state` to
    /// `canceled` for pending/running rows; the worker observes the
    /// state change on its next poll and SIGTERMs its child.
    async fn request_cancel_job(&self, _job_id: &str) -> Result<(), RepoError> {
        Err(RepoError::ReadOnly)
    }

    // --- Worker-side job lifecycle ------------------------------------
    //
    // These are the writes the in-process sync worker issues as it drains
    // the queue. The HTTP request handlers never call them; only
    // `worker::run` does. Default impls return [`RepoError::ReadOnly`] so
    // a read-only backend simply never makes progress on jobs.

    /// Atomically claim the oldest `pending` job: flip it to `running`,
    /// stamp `started_at`, and return the updated row. Returns `Ok(None)`
    /// when the queue is empty. Single-worker by construction, so the
    /// SELECT-then-UPDATE needs no extra locking beyond SQLite's
    /// single-writer guarantee.
    async fn claim_next_job(&self) -> Result<Option<SyncJobRow>, RepoError> {
        Err(RepoError::ReadOnly)
    }

    /// Record the OS pid of the child process driving a `running` job, so
    /// a future worker restart can detect orphaned rows.
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

    /// Move a job to a terminal state (`done` / `failed` / `canceled`),
    /// stamping `finished_at`, clearing `pid`, and recording an optional
    /// error summary.
    async fn finish_job(
        &self,
        _job_id: &str,
        _state: &str,
        _error: Option<&str>,
    ) -> Result<(), RepoError> {
        Err(RepoError::ReadOnly)
    }

    /// Startup recovery: flip any rows still marked `running` (left over
    /// from a previous backend process that died mid-job) to `failed`.
    /// Returns the number of rows recovered.
    async fn recover_running_jobs(&self) -> Result<usize, RepoError> {
        Err(RepoError::ReadOnly)
    }

    // --- The disk-usage timeseries ------------------------------------

    /// Append disk-usage samples. The caller has already applied the
    /// compaction rules (drop an unchanged value; never two samples for
    /// one series within five seconds) — this only writes.
    ///
    /// Deliberately not versioned: `disk_usage` rows *are* the history,
    /// so a `dolt_commit` per sample would flood `dolt_log` and record
    /// nothing the table doesn't already say.
    async fn record_disk_usage(&self, _rows: &[DiskUsageRow]) -> Result<(), RepoError> {
        Err(RepoError::ReadOnly)
    }

    /// The newest `limit` disk-usage samples across every series,
    /// newest first. Used to seed the in-memory window the sparklines
    /// draw, so a server restart doesn't blank them for five minutes.
    async fn recent_disk_usage(&self, _limit: usize) -> Result<Vec<DiskUsageRow>, RepoError> {
        Err(RepoError::ReadOnly)
    }
}

/// Convenience alias for the dyn-dispatched app-store handle used by
/// HTTP handlers via `axum::State`.
pub type DynAppRepo = Arc<dyn AppRepo>;
