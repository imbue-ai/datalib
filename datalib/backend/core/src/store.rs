//! Opening a doltlite file the app server owns. A reader of somebody
//! else's store opens through `datalib_pin` instead.

use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

pub async fn open_pool(db_path: &std::path::Path) -> Result<SqlitePool, sqlx::Error> {
    if let Some(parent) = db_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))?
        .create_if_missing(true)
        // WAL / NORMAL synchronous are no-ops on doltlite (its chunk
        // store ignores the SQLite pager journal), but harmless to leave
        // as documentation of intent for stock-libsqlite3 builds (e.g.
        // cargo-only unit tests).
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
}
