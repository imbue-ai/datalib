//! `MirrorRepo` is the single seam between the HTTP layer and the
//! underlying SQL store.
//!
//! Implementations:
//! * [`crate::dolt_repo::DoltRepo`] — `sqlx::MySqlPool` against the
//!   managed `dolt sql-server`. Production path; the only writable
//!   backend; default in T7.
//! * [`crate::sqlite_repo::SqliteRepo`] — `sqlx::SqlitePool` against
//!   the periodically-materialized `<root>/mirror.sqlite`. Read-only
//!   reference impl, reachable via an explicit CLI flag in T7.
//!
//! Why a trait? Per the feedback-mechanism plan (component C2), the
//! running app is moving its source-of-truth from `mirror.sqlite` to a
//! managed Dolt repo. The trait lets us flip the cutover at startup and
//! keep the SQLite path around as a debug / backwards-compat reference.
//!
//! All methods are async because both impls run on top of sqlx pools.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;

use crate::db::ChatMeta;
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
#[async_trait]
pub trait MirrorRepo: Send + Sync {
    /// Run a grid-search query and return rows for the UI.
    async fn search(&self, query: &ParsedQuery, limit: usize) -> Result<Vec<SearchRow>, RepoError>;

    /// Fetch the per-conversation header data. Returns `Ok(None)` when
    /// no chat-level row exists for the UUID.
    async fn chat_meta(&self, conversation_uuid: &str) -> Result<Option<ChatMeta>, RepoError>;

    /// Resolve the on-disk QMD path for a conversation. The returned
    /// path is absolute (already joined with the data root).
    async fn qmd_path_for_conversation(
        &self,
        conversation_uuid: &str,
    ) -> Result<Option<PathBuf>, RepoError>;
}

/// Convenience type alias for the dyn-dispatched repo handle used by
/// HTTP handlers via `axum::State`.
pub type DynRepo = Arc<dyn MirrorRepo>;

/// Default repo factory. Today: opens the SQLite mirror at
/// `<root>/mirror.sqlite` read-only. T7 flips this default to
/// `DoltRepo` and exposes SQLite behind a debug CLI flag.
///
/// If `mirror.sqlite` is missing we still return a working repo —
/// it just yields zero rows on every query, matching the previous
/// `rusqlite`-era behavior. We materialize this by lazily-opening a
/// pool against an empty in-memory DB so the trait dispatch keeps
/// working.
pub async fn default_repo(root: Arc<PathBuf>) -> DynRepo {
    use crate::sqlite_repo::SqliteRepo;
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    let db_path = root.as_ref().join("mirror.sqlite");
    if db_path.exists() {
        if let Ok(repo) = SqliteRepo::open(root.clone()).await {
            return Arc::new(repo);
        }
    }
    // Fallback: empty in-memory DB with the grid_rows DDL applied so
    // SELECTs succeed and return zero rows.
    let opts = SqliteConnectOptions::from_str("sqlite::memory:").unwrap();
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .expect("in-memory sqlite always opens");
    for (_table, ddl) in frankweiler_schema::grid_rows::DDL {
        let _ = sqlx::query(ddl).execute(&pool).await;
    }
    Arc::new(SqliteRepo::from_pool(pool, root))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn default_repo_on_missing_root_returns_empty_results() {
        let root = Arc::new(PathBuf::from("/tmp/fw-no-such-root-for-tests"));
        let repo = default_repo(root).await;
        let parsed = crate::query::parse_query("");
        let out = repo.search(&parsed, 10).await.unwrap();
        assert!(out.is_empty());
    }
}
