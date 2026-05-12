//! `MirrorRepo` is the single seam between the HTTP layer and the
//! underlying SQL store. Today there's one implementation:
//! [`LegacySqliteRepo`], which is a thin async wrapper around the
//! existing rusqlite-backed functions in [`crate::db`]. T5 adds
//! `DoltRepo` (sqlx::MySqlPool against the managed `dolt sql-server`)
//! and T6 replaces this legacy impl with a sqlx-based one.
//!
//! Why a trait? Per the feedback-mechanism plan (component C2), the
//! running app is moving its source-of-truth from `mirror.sqlite`
//! (read-only, periodically re-materialized by ingest) to a managed
//! Dolt repo (writable, mutated by the app itself). The trait lets us
//! flip the cutover at startup and keep the SQLite path around as a
//! debug / backwards-compat reference.
//!
//! All methods are async so the production [`crate::dolt_server`] +
//! `sqlx::MySqlPool` impl is natural. Sync rusqlite work in the legacy
//! impl is parked on a blocking task via `tokio::task::spawn_blocking`.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::db::{self, ChatMeta};
use crate::query::ParsedQuery;
use crate::search::SearchRow;

#[derive(Debug, thiserror::Error)]
pub enum RepoError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("backend does not support write operations (running on read-only SQLite)")]
    ReadOnly,
    #[error("internal: {0}")]
    Internal(String),
}

/// The single point that all backend SQL flows through.
///
/// Implementations: [`LegacySqliteRepo`] today; `DoltRepo`/`SqliteRepo`
/// in T5/T6.
#[async_trait]
pub trait MirrorRepo: Send + Sync {
    /// Run a grid-search query and return rows for the UI.
    async fn search(&self, query: &ParsedQuery, limit: usize) -> Result<Vec<SearchRow>, RepoError>;

    /// Fetch the per-conversation header data (name, account, project, ...).
    /// Returns `Ok(None)` when no chat-level row exists for the UUID.
    async fn chat_meta(&self, conversation_uuid: &str) -> Result<Option<ChatMeta>, RepoError>;

    /// Resolve the on-disk QMD path for a conversation. Returned path
    /// is absolute (already joined with the data root).
    async fn qmd_path_for_conversation(
        &self,
        conversation_uuid: &str,
    ) -> Result<Option<PathBuf>, RepoError>;
}

/// Wraps the existing rusqlite-backed `db::*` functions behind the
/// async trait. Sync work runs in `spawn_blocking`. T6 deletes this
/// in favor of a sqlx::SqlitePool-based implementation.
pub struct LegacySqliteRepo {
    root: Arc<PathBuf>,
}

impl LegacySqliteRepo {
    pub fn new(root: Arc<PathBuf>) -> Self {
        Self { root }
    }
}

#[async_trait]
impl MirrorRepo for LegacySqliteRepo {
    async fn search(&self, query: &ParsedQuery, limit: usize) -> Result<Vec<SearchRow>, RepoError> {
        let root = self.root.clone();
        let query = query.clone();
        tokio::task::spawn_blocking(move || db::grid_rows(root.as_ref().as_path(), &query, limit))
            .await
            .map_err(|e| RepoError::Internal(e.to_string()))
    }

    async fn chat_meta(&self, conversation_uuid: &str) -> Result<Option<ChatMeta>, RepoError> {
        let root = self.root.clone();
        let uuid = conversation_uuid.to_string();
        tokio::task::spawn_blocking(move || db::chat_meta(root.as_ref().as_path(), &uuid))
            .await
            .map_err(|e| RepoError::Internal(e.to_string()))
    }

    async fn qmd_path_for_conversation(
        &self,
        conversation_uuid: &str,
    ) -> Result<Option<PathBuf>, RepoError> {
        let root = self.root.clone();
        let uuid = conversation_uuid.to_string();
        tokio::task::spawn_blocking(move || {
            db::qmd_path_for_conversation(root.as_ref().as_path(), &uuid)
        })
        .await
        .map_err(|e| RepoError::Internal(e.to_string()))
    }
}

/// Convenience type alias for the dyn-dispatched repo handle used by
/// HTTP handlers via `axum::State`.
pub type DynRepo = Arc<dyn MirrorRepo>;

/// Build the default repo for the given data root. Today this is the
/// legacy SQLite-backed impl; T7 changes the default to `DoltRepo`.
pub fn default_repo(root: Arc<PathBuf>) -> DynRepo {
    Arc::new(LegacySqliteRepo::new(root))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tokio::runtime::Runtime;

    #[test]
    fn legacy_repo_search_on_missing_root_returns_empty() {
        // No mirror.sqlite → existing db::grid_rows returns Vec::new();
        // we surface that as Ok(vec![]) through the trait.
        let rt = Runtime::new().unwrap();
        let root = Arc::new(PathBuf::from("/tmp/fw-no-such-root-for-tests"));
        let repo = LegacySqliteRepo::new(root);
        let parsed = crate::query::parse_query("");
        let out = rt.block_on(repo.search(&parsed, 10)).unwrap();
        assert!(out.is_empty());
    }
}
