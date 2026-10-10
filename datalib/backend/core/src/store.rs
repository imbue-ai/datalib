//! Opening a file the app server owns: a doltlite store, or the one plain
//! SQLite file (`disk_stats`). A reader of somebody else's store opens
//! through `datalib_pin` instead.

use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

pub async fn open_pool(db_path: &std::path::Path) -> Result<SqlitePool, sqlx::Error> {
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))?
        .create_if_missing(true)
        // Meant for stock-libsqlite3 builds. On doltlite `journal_mode`
        // is inert (docs/dev/doltlite.md#plain-sqlite-files-and-sqlite-compatibility),
        // and any `synchronous` above OFF syncs every commit.
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);
    SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts)
        .await
}

/// A plain SQLite file, created as one (`plain_sqlite::uri`). On the
/// stock engine `journal_mode` is honoured, and rollback-journal mode is
/// what it settles in, so ask for that.
pub async fn open_plain_pool(db_path: &std::path::Path) -> Result<SqlitePool, sqlx::Error> {
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let opts = SqliteConnectOptions::new()
        .filename(crate::plain_sqlite::uri(db_path))
        .create_if_missing(true)
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Delete)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .busy_timeout(std::time::Duration::from_secs(10));
    SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts)
        .await
}
