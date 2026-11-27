//! Opening a plain SQLite file this crate writes: WAL, one connection, its
//! tables created. The inference cache is a file of its own, kept in this
//! build's shape here; the keyword index's tables are in the grid index's
//! file, whose shape the grid index's writer keeps (`grid_index::shape`).

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use datalib_sql::{sql, sql_id, Sql};
use datalib_store_meta::{Shape, StoreKind};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};

/// `path`, created when missing, with every statement of `ddl` run.
pub(crate) async fn open(path: &Path, ddl: &[&str], wait: Duration) -> Result<SqlitePool> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let opts = SqliteConnectOptions::new()
        .filename(datalib_runtime::plain_sqlite::uri(path))
        .create_if_missing(true)
        // Search reads the last committed batch while a step writes the next.
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .auto_vacuum(sqlx::sqlite::SqliteAutoVacuum::Incremental)
        // Takes effect only on a new file (`cache::PAGE_SIZE`).
        .page_size(crate::cache::PAGE_SIZE as u32)
        .pragma("temp_store", "memory")
        .pragma(
            "journal_size_limit",
            datalib_runtime::plain_sqlite::WAL_SIZE_LIMIT_BYTES,
        )
        .busy_timeout(wait);
    // The pool waits as long as SQLite does: its own default (30s) would
    // give up on the connection's first statement while the grid index's
    // step still holds the file, whatever `busy_timeout` says.
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(wait)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts)
        .await
        .with_context(|| format!("open {}", path.display()))?;
    for stmt in ddl {
        // Audited: every statement is one of this crate's constants.
        if let Err(e) = Sql::dynamic(sql_id!("search_index.file.create_table_on_open"), *stmt)
            .query()
            .execute(&pool)
            .await
        {
            pool.close().await;
            return Err(e).with_context(|| format!("create the tables of {}", path.display()));
        }
    }
    Ok(pool)
}

/// Make the file at `pool` hold `kind` in `shape`, under the write lock, so
/// two steps opening an old file together empty it once. Tables in another
/// shape, or a file that records none, lose every table; indexes in
/// another shape lose the `remade` tables, which are made again from what
/// the rest hold. `ddl` then makes whatever is missing.
pub(crate) async fn keep_in_shape(
    pool: &SqlitePool,
    kind: StoreKind,
    shape: &Shape,
    ddl: &[&str],
    remade: &[&str],
) -> Result<()> {
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let have = datalib_store_meta::read_in(&mut tx)
        .await?
        .map(|m| m.shape());
    if have.as_ref().map(|h| &h.tables) != Some(&shape.tables) {
        tracing::info!(
            store = kind.as_str(),
            "written in another shape; emptying it"
        );
        drop_tables(&mut tx).await?;
    } else if have.as_ref().map(|h| &h.indexes) != Some(&shape.indexes) {
        for table in remade {
            // Audited: a name from this crate's constants.
            let sql = Sql::dynamic(
                sql_id!("search_index.file.drop_remade_table"),
                format!("DROP TABLE IF EXISTS {table}"),
            );
            sql.query().execute(&mut *tx).await?;
        }
    }
    for stmt in ddl {
        // Audited: every statement is one of this crate's constants.
        Sql::dynamic(sql_id!("search_index.file.create_table_in_shape"), *stmt)
            .query()
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    datalib_store_meta::write(pool, kind, shape).await?;
    Ok(())
}

/// Every table but `_datalib_meta`, virtual tables first so their shadow
/// tables go with them.
async fn drop_tables(conn: &mut sqlx::SqliteConnection) -> Result<()> {
    for virtual_only in [true, false] {
        let names: Vec<String> = sql!(
            "search_index.file.list_tables",
            "SELECT name FROM sqlite_master WHERE type = 'table' \
               AND name NOT LIKE 'sqlite_%' AND name <> '_datalib_meta' \
               AND (NOT ? OR sql LIKE 'CREATE VIRTUAL TABLE%')"
        )
        .query_scalar()
        .bind(virtual_only)
        .fetch_all(&mut *conn)
        .await?;
        for name in names {
            // A name read from this file's own schema, quoted as an identifier.
            let sql = Sql::dynamic(
                sql_id!("search_index.file.drop_table"),
                format!("DROP TABLE IF EXISTS \"{}\"", name.replace('"', "\"\"")),
            );
            sql.query().execute(&mut *conn).await?;
        }
    }
    Ok(())
}
