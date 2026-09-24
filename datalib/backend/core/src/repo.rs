//! [`AppRepo`] — the seam to the stores this server owns: filed
//! feedback, disk usage and remote media.

use std::sync::Arc;

use async_trait::async_trait;

use app_schema::disk_usage::DiskUsageRow;
use app_schema::feedback::FeedbackRow;
use app_schema::remote_media::allow::RemoteMediaAllowRow;
use app_schema::remote_media::media::RemoteMediaRow;
use app_schema::remote_media::AllowScope;

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("backend does not support write operations")]
    ReadOnly,
    #[error("internal: {0}")]
    Internal(String),
}

/// Writes and reads of the application stores.
#[async_trait]
pub trait AppRepo: Send + Sync {
    /// Append a feedback row. The default impl returns
    /// [`RepoError::ReadOnly`]; only [`crate::dolt_repo::AppStore`]
    /// overrides it.
    async fn insert_feedback(&self, _row: FeedbackRow) -> Result<(), RepoError> {
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

    // --- Remote media: the allow-list and the download CAS's index ----

    async fn list_remote_allows(&self) -> Result<Vec<RemoteMediaAllowRow>, RepoError> {
        Err(RepoError::ReadOnly)
    }

    /// Record a decision to load; the row already there for the same
    /// `(scope, key)` comes back rather than a second one.
    async fn allow_remote(
        &self,
        _scope: AllowScope,
        _key: &str,
    ) -> Result<RemoteMediaAllowRow, RepoError> {
        Err(RepoError::ReadOnly)
    }

    /// Whether a row was there to delete.
    async fn delete_remote_allow(&self, _allow_uuid: &str) -> Result<bool, RepoError> {
        Err(RepoError::ReadOnly)
    }

    async fn get_remote_media(&self, _url: &str) -> Result<Option<RemoteMediaRow>, RepoError> {
        Err(RepoError::ReadOnly)
    }

    async fn list_remote_media(&self) -> Result<Vec<RemoteMediaRow>, RepoError> {
        Err(RepoError::ReadOnly)
    }

    /// Record a URL fetched into the CAS; a second fetch of the same
    /// URL replaces the row.
    async fn record_remote_media(&self, _row: RemoteMediaRow) -> Result<(), RepoError> {
        Err(RepoError::ReadOnly)
    }
}

/// Convenience alias for the dyn-dispatched app-store handle used by
/// HTTP handlers via `axum::State`.
pub type DynAppRepo = Arc<dyn AppRepo>;
