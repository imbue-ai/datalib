//! Opening this crate's tables in a plain SQLite file: WAL, one
//! connection, and the tables in this build's shape. Tables in another
//! shape are emptied, since everything in them can be made again. The
//! inference cache is a file of its own, with the embedded chunks a part
//! of it; the index is a part of the grid index's file. A part empties
//! only its own tables.

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use datalib_store_meta::StoreKind;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::{AssertSqlSafe, SqliteConnection};

/// Which tables of the file are ours, and where our shape is recorded.
#[derive(Clone, Copy)]
pub(crate) enum Owns {
    /// Every table: the file's `_datalib_meta` is ours.
    File(StoreKind),
    /// The tables whose names start with [`crate::TABLE_PREFIX`]; our shape
    /// is our own key in the file's `_datalib_meta`.
    Part(StoreKind),
}

impl Owns {
    fn tables_like(self) -> String {
        match self {
            Owns::File(_) => "%".to_string(),
            Owns::Part(_) => format!("{}%", crate::TABLE_PREFIX.replace('_', "\\_")),
        }
    }
}

/// One owner's tables in a file: their DDL, the indexes made after them
/// (recorded apart, as the file's `index_hash`, so adding one costs making
/// it rather than emptying the tables), and which tables are its own.
pub(crate) struct Part<'a> {
    pub ddl: &'a [&'static str],
    pub indexes: &'a [&'static str],
    pub owns: Owns,
}

/// Opens `path` with each of `parts` in this build's shape, in order: a
/// file's own tables before a part of it, so emptying the file leaves
/// the part to make its tables again.
pub(crate) async fn open(path: &Path, parts: &[Part<'_>], wait: Duration) -> Result<SqlitePool> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
    }
    let pool = connect(path, wait).await?;
    for part in parts {
        if let Err(e) = set_up(&pool, part).await {
            pool.close().await;
            return Err(e);
        }
    }
    Ok(pool)
}

async fn connect(path: &Path, wait: Duration) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(datalib_runtime::plain_sqlite::uri(path))
        .create_if_missing(true)
        // Search reads the last committed batch while a step writes the next.
        .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
        .synchronous(sqlx::sqlite::SqliteSynchronous::Normal)
        .auto_vacuum(sqlx::sqlite::SqliteAutoVacuum::Incremental)
        .pragma("temp_store", "memory")
        .pragma(
            "journal_size_limit",
            datalib_runtime::plain_sqlite::WAL_SIZE_LIMIT_BYTES,
        )
        .busy_timeout(wait);
    // The pool waits as long as SQLite does: its own default (30s) would
    // give up on the connection's first statement while the grid index's
    // step still holds the file, whatever `busy_timeout` says.
    SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(wait)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts)
        .await
        .with_context(|| format!("open {}", path.display()))
}

/// The shape `part` records in its file.
pub(crate) fn shape_of(part: &Part<'_>) -> datalib_store_meta::Shape {
    datalib_store_meta::Shape {
        tables: datalib_store_meta::schema_hash(part.ddl.iter().copied()),
        indexes: (!part.indexes.is_empty())
            .then(|| datalib_store_meta::schema_hash(part.indexes.iter().copied())),
        versions: datalib_store_meta::Versions::default(),
    }
}

/// Brings our tables to this build's shape under the write lock, so two
/// steps opening an old file together empty it once, and neither empties
/// what the other has started writing: the second finds the new shape. A
/// file that records no shape for them is in none, and emptied too.
async fn set_up(pool: &SqlitePool, part: &Part<'_>) -> Result<()> {
    let (ddl, owns) = (part.ddl, part.owns);
    let shape = shape_of(part);
    let want = shape.tables.clone();
    let mut tx = pool.begin_with("BEGIN IMMEDIATE").await?;
    let have = match owns {
        Owns::File(_) => datalib_store_meta::schema_hash_in(&mut tx).await?,
        Owns::Part(part) => datalib_store_meta::part_schema_hash_in(&mut tx, part).await?,
    };
    if have.as_deref() != Some(want.as_str()) {
        let (Owns::File(kind) | Owns::Part(kind)) = owns;
        tracing::info!(
            store = kind.as_str(),
            "written in another shape; emptying it"
        );
        drop_tables(&mut tx, owns).await?;
    }
    for stmt in ddl.iter().chain(part.indexes) {
        sqlx::query(*stmt).execute(&mut *tx).await?;
    }
    if let Owns::Part(part) = owns {
        datalib_store_meta::write_part_schema_hash(&mut tx, part, &want).await?;
    }
    tx.commit().await?;
    if let Owns::File(kind) = owns {
        datalib_store_meta::write(pool, kind, &shape).await?;
    }
    Ok(())
}

/// Every table we own but `_datalib_meta`, virtual tables first so their
/// shadow tables go with them.
pub(crate) async fn drop_tables(conn: &mut SqliteConnection, owns: Owns) -> Result<()> {
    for virtual_only in [true, false] {
        let names: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' \
               AND name NOT LIKE 'sqlite_%' AND name <> '_datalib_meta' \
               AND name LIKE ? ESCAPE '\\' \
               AND (NOT ? OR sql LIKE 'CREATE VIRTUAL TABLE%')",
        )
        .bind(owns.tables_like())
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
