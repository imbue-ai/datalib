//! Opening one of this crate's plain SQLite files: WAL, one connection,
//! and its tables in this build's shape. A file in another shape is
//! emptied, since everything in it can be made again.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use datalib_store_meta::StoreKind;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::{AssertSqlSafe, SqliteConnection};

pub(crate) async fn open(path: &Path, ddl: &[&'static str], kind: StoreKind) -> Result<SqlitePool> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let pool = connect(path).await?;
    match set_up(&pool, ddl, kind).await {
        Ok(()) => Ok(pool),
        Err(e) => {
            pool.close().await;
            Err(e)
        }
    }
}

async fn connect(path: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(datalib_runtime::plain_sqlite::uri(path))
        .create_if_missing(true)
        // Search reads the last committed batch while a step writes the next.
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .auto_vacuum(sqlx::sqlite::SqliteAutoVacuum::Incremental)
        .pragma("temp_store", "memory")
        // Every source's steps write this one file, a batch at a time.
        .busy_timeout(Duration::from_secs(60));
    SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts)
        .await
        .with_context(|| format!("open {}", path.display()))
}

/// Brings the file to this build's shape under the write lock, so two
/// steps opening an old file together empty it once, and neither empties
/// what the other has started writing: the second finds the new shape.
async fn set_up(pool: &SqlitePool, ddl: &[&'static str], kind: StoreKind) -> Result<()> {
    let want = datalib_store_meta::schema_hash(ddl.iter().copied());
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let have = datalib_store_meta::schema_hash_in(&mut tx).await?;
    if have.as_ref().is_some_and(|h| *h != want) {
        tracing::info!(
            store = kind.as_str(),
            "written in another shape; emptying it"
        );
        drop_tables(&mut tx).await?;
    }
    for stmt in ddl {
        sqlx::query(*stmt).execute(&mut *tx).await?;
    }
    tx.commit().await?;
    datalib_store_meta::write(pool, kind, &want, datalib_store_meta::Versions::default()).await?;
    Ok(())
}

/// Every table but `_datalib_meta`, virtual tables first so their shadow
/// tables go with them.
async fn drop_tables(conn: &mut SqliteConnection) -> Result<()> {
    for virtual_only in [true, false] {
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' \
               AND name NOT LIKE 'sqlite_%' AND name <> '_datalib_meta' \
               AND (NOT ? OR sql LIKE 'CREATE VIRTUAL TABLE%')",
        )
        .bind(virtual_only)
        .fetch_all(&mut *conn)
        .await?;
        for name in names {
            // A name read from this file's own schema, quoted as an identifier.
            let sql = format!("DROP TABLE IF EXISTS \"{}\"", name.replace('"', "\"\""));
            sqlx::query(AssertSqlSafe(sql)).execute(&mut *conn).await?;
        }
    }
    Ok(())
}
