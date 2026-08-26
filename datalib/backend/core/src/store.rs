//! Opening a doltlite file, and the one error every reader must
//! tolerate. Shared by the app stores here and by the grid index in
//! `datalib_unified_index`, so the two cannot drift on pool settings.

use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

/// Open (or create) one doltlite file with the settings every store in
/// this codebase uses.
///
/// Pool size 1: doltlite's per-connection HEAD pointer means a pool
/// wider than one connection produces silent `dolt_log` dropouts and
/// `commit conflict` errors on interleaved writes. See
/// `datalib_etl::doltlite_raw` module docs for the full story
/// (dolt-team-confirmed advice). Splitting one database into three does
/// not relax it: the working set is shared, so two connections on one
/// file are no safer than they were.
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

/// True iff `e` is SQLite's "no such table: <table>" for exactly the
/// given table — the fresh-data-root state, before whatever step owns
/// that table has run for the first time. Readers map this one case to
/// "no data yet". Deliberately narrow: an exact message match on the
/// single table the query reads, so real failures — corruption, bad
/// SQL, missing columns, connection errors — still surface as errors.
pub fn is_missing_table(e: &sqlx::Error, table: &str) -> bool {
    match e {
        sqlx::Error::Database(db) => db.message() == format!("no such table: {table}"),
        _ => false,
    }
}
