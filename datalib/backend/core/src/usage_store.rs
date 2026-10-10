//! The usage store, `system/usage.sqlite`: a plain SQLite file, and the
//! one-time move of the doltlite store an older build kept the same rows
//! in. Nothing ever committed that store, so its commit log held nothing,
//! and every write left the pages it replaced in the file for good.

use std::path::Path;

use app_schema::disk_usage::DDL as DISK_USAGE_DDL;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePool, SqlitePoolOptions};

use crate::app_store_migrate::DISK_USAGE_LADDER;

/// Where the timeseries lived while it was a doltlite store, beside
/// [`crate::layout::USAGE_DB`]. Kept, with [`carry_forward`], as the
/// forward path for the rows an older build left there (AGENTS.md
/// § "Keep a forward path for existing data").
pub(crate) const DOLTLITE_USAGE: &str = "usage.doltlite_db";

pub(crate) async fn open(path: &Path) -> Result<SqlitePool, sqlx::Error> {
    carry_forward(path).await?;
    connect(path).await
}

async fn connect(path: &Path) -> Result<SqlitePool, sqlx::Error> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(sqlx::Error::Io)?;
    }
    let opts = SqliteConnectOptions::new()
        .filename(datalib_runtime::plain_sqlite::uri(path))
        .create_if_missing(true)
        // What the plain-SQLite engine really does: asked for WAL it
        // answers `wal` and stays in rollback-journal mode.
        .journal_mode(SqliteJournalMode::Delete);
    SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts)
        .await
}

/// Moves the rows of a doltlite usage store into the plain file at
/// `path`, then deletes the old store. Deleted rather than renamed aside,
/// as the blob CAS's move did: every row is in the new file, counted, and
/// the old file's size is the problem this move exists to fix. A plain
/// file already there means an earlier open copied the rows and stopped
/// before the delete, so the old store only goes.
async fn carry_forward(path: &Path) -> Result<(), sqlx::Error> {
    let old = path.with_file_name(DOLTLITE_USAGE);
    if !old.exists() {
        return Ok(());
    }
    if !path.exists() {
        copy(&old, path).await?;
    }
    for leftover in [
        old.clone(),
        old.with_file_name(format!("{DOLTLITE_USAGE}.lock")),
        old.with_file_name(format!(".{DOLTLITE_USAGE}-lock")),
    ] {
        match std::fs::remove_file(&leftover) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(sqlx::Error::Io(e)),
            _ => {}
        }
    }
    Ok(())
}

/// The copy goes to a temporary file and is renamed into place only once
/// its row count matches, so a crash leaves the old store whole and the
/// next open starts over.
async fn copy(old: &Path, path: &Path) -> Result<(), sqlx::Error> {
    let tmp = path.with_extension("sqlite.tmp");
    for stale in [tmp.clone(), tmp.with_extension("tmp-journal")] {
        match std::fs::remove_file(&stale) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(sqlx::Error::Io(e)),
            _ => {}
        }
    }
    let out = connect(&tmp).await?;
    let made = create_table(&out).await;
    out.close().await;
    made?;

    let pool = crate::store::open_pool(old).await?;
    let copied = async {
        // The old store climbs its own ladder first, so the rows copied
        // are in the shape this build reads.
        crate::app_store::climb(&pool, old, DISK_USAGE_LADDER, false).await?;
        create_table(&pool).await?;
        // Safe: the URI is our own path, quoted.
        let attach = format!(
            "ATTACH '{}' AS out",
            datalib_runtime::plain_sqlite::uri(&tmp).replace('\'', "''")
        );
        sqlx::query(sqlx::AssertSqlSafe(attach))
            .execute(&pool)
            .await?;
        sqlx::query(
            "INSERT INTO out.disk_usage (path, measured_at_utc, tz_offset, bytes) \
             SELECT path, measured_at_utc, tz_offset, bytes FROM main.disk_usage",
        )
        .execute(&pool)
        .await?;
        let (rows_old, rows_new): (i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM main.disk_usage), (SELECT count(*) FROM out.disk_usage)",
        )
        .fetch_one(&pool)
        .await?;
        sqlx::query("DETACH out").execute(&pool).await?;
        if rows_old != rows_new {
            return Err(sqlx::Error::Protocol(format!(
                "the copy of {} holds {rows_new} disk_usage rows, the old store {rows_old}",
                old.display()
            )));
        }
        tracing::info!(
            rows = rows_new,
            from = %old.display(),
            "moved the usage store to plain SQLite"
        );
        Ok(())
    }
    .await;
    pool.close().await;
    copied?;
    std::fs::rename(&tmp, path).map_err(sqlx::Error::Io)
}

pub(crate) async fn create_table(pool: &SqlitePool) -> Result<(), sqlx::Error> {
    for (_table, ddl) in DISK_USAGE_DDL {
        sqlx::query(*ddl).execute(pool).await?;
    }
    Ok(())
}
