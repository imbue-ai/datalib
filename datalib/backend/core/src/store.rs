//! Opening a doltlite file, and the one error every reader must
//! tolerate. Shared by the app stores here and by the grid index in
//! `datalib_unified_index`, so the two cannot drift on pool settings.

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

/// A reader's handle on a store some other process writes: read-only,
/// never creates the file, and keeps its one connection rather than
/// recycling it (a fresh connection on a doltlite file is a second
/// handle on the same working set). `Err` when the file is absent —
/// a caller that can run before the writer's first pass has to expect
/// that and answer "no data yet" itself.
pub async fn open_pool_read_only(db_path: &std::path::Path) -> Result<SqlitePool, sqlx::Error> {
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))?
        .read_only(true)
        .create_if_missing(false);
    SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
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

#[cfg(test)]
mod tests {
    use super::*;

    async fn count(pool: &SqlitePool) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM t")
            .fetch_one(pool)
            .await
            .unwrap()
    }

    async fn commit(pool: &SqlitePool) {
        let hash: Option<String> = sqlx::query_scalar("SELECT dolt_commit('-Am', 'c')")
            .fetch_one(pool)
            .await
            .unwrap();
        assert!(hash.is_some(), "doltlite linked");
    }

    async fn create_and_insert(pool: &SqlitePool, id: i64) {
        sqlx::query("CREATE TABLE IF NOT EXISTS t (id INTEGER PRIMARY KEY)")
            .execute(pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO t VALUES (?)")
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
    }

    /// The search applet holds one read-only handle for the life of the
    /// server and has to see every pass the `grid_index` step makes after
    /// it opened. It does — the working set is the file's, not the
    /// connection's — and it sees the writer's uncommitted rows too, which
    /// is why a per-request pin is still the next step.
    #[tokio::test]
    async fn a_read_only_pool_sees_the_writers_later_rows() {
        let td = tempfile::tempdir().unwrap();
        let db = td.path().join("t.doltlite_db");
        let writer = open_pool(&db).await.unwrap();
        create_and_insert(&writer, 1).await;
        commit(&writer).await;

        let reader = open_pool_read_only(&db).await.unwrap();
        assert_eq!(count(&reader).await, 1);
        create_and_insert(&writer, 2).await;
        assert_eq!(count(&reader).await, 2, "the working set");
        commit(&writer).await;
        assert_eq!(count(&reader).await, 2, "the commit");
    }

    /// The applet starts before the first sync on a fresh root: opened on
    /// a store with no tables and no commits, the handle still sees what
    /// the writer creates afterwards.
    #[tokio::test]
    async fn a_read_only_pool_opened_on_an_empty_store_sees_what_comes_later() {
        let td = tempfile::tempdir().unwrap();
        let db = td.path().join("t.doltlite_db");
        let writer = open_pool(&db).await.unwrap();
        let reader = open_pool_read_only(&db).await.unwrap();
        create_and_insert(&writer, 1).await;
        commit(&writer).await;
        assert_eq!(count(&reader).await, 1);
    }

    /// A reader must never be the thing that creates the writer's file.
    #[tokio::test]
    async fn a_read_only_pool_does_not_create_the_file() {
        let td = tempfile::tempdir().unwrap();
        let db = td.path().join("absent.doltlite_db");
        assert!(open_pool_read_only(&db).await.is_err());
        assert!(!db.exists());
    }
}
