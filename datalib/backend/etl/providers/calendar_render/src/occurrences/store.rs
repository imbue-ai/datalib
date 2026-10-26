//! The occurrences store, `<group>/render_occurrences/occurrences.doltlite_db`:
//! its tables, and the one write that replaces what it holds.

use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::doltlite_raw::{self, StoreKind};
use datalib_etl_calendar_common::occurrences::Window;
use sqlx::SqlitePool;

use super::{Listed, OccurrenceRow};

const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS occurrences (
        id TEXT PRIMARY KEY,
        calendar_id TEXT NOT NULL,
        instance_id TEXT NOT NULL,
        series_id TEXT NULL,
        original_start_time TEXT NULL,
        start_time TEXT NOT NULL,
        end_time TEXT NULL,
        start_utc TEXT NOT NULL,
        end_utc TEXT NOT NULL,
        all_day INTEGER NOT NULL,
        time_zone TEXT NULL,
        status TEXT NOT NULL,
        title TEXT NULL,
        event_row TEXT NOT NULL,
        payload TEXT NOT NULL
    )",
    "CREATE TABLE IF NOT EXISTS unlisted (
        event_row TEXT NOT NULL,
        calendar_id TEXT NOT NULL,
        reason TEXT NOT NULL,
        PRIMARY KEY (event_row, reason)
    )",
    "CREATE TABLE IF NOT EXISTS occurrences_window (
        id INTEGER PRIMARY KEY CHECK (id = 1),
        from_utc TEXT NOT NULL,
        until_utc TEXT NOT NULL
    )",
];

const INDEXES: &[&str] = &[
    "CREATE INDEX IF NOT EXISTS occurrences_by_start ON occurrences(start_utc)",
    "CREATE INDEX IF NOT EXISTS occurrences_by_series ON occurrences(calendar_id, series_id)",
];

const COLUMNS: &[&str] = &[
    "id",
    "calendar_id",
    "instance_id",
    "series_id",
    "original_start_time",
    "start_time",
    "end_time",
    "start_utc",
    "end_utc",
    "all_day",
    "time_zone",
    "status",
    "title",
    "event_row",
    "payload",
];

/// One row's placeholders, one per column.
const ONE_ROW: &str = "(?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)";

/// Rows per `INSERT`, well under SQLite's 32,766 bound values.
const CHUNK: usize = 1000;

/// Open `db_path` as its one writer, in this build's shape; a store in
/// another is rebuilt empty. The schema is committed before this
/// returns.
pub async fn open(db_path: &Path) -> Result<SqlitePool> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let pool = doltlite_raw::open_derived_indexed(db_path, DDL, INDEXES, StoreKind::Occurrences)
        .await
        .with_context(|| format!("open the occurrences store at {}", db_path.display()))?;
    let versions = datalib_store_meta::Versions {
        schema: 0,
        shared: 0,
    };
    let opened = async {
        datalib_store_meta::write(&pool, StoreKind::Occurrences, &schema_hash(), versions)
            .await
            .context("write _datalib_meta for the occurrences store")?;
        doltlite_raw::commit_run(&pool, "schema: occurrences")
            .await
            .context("commit the occurrences schema")
    }
    .await;
    if let Err(e) = opened {
        pool.close().await;
        return Err(e);
    }
    Ok(pool)
}

/// The shape `open` records in `_datalib_meta`: what a launch's migrate
/// pass compares to decide whether the store must be rebuilt.
pub fn schema_hash() -> String {
    doltlite_raw::recorded_shape(DDL.iter().copied())
}

/// Make the store hold `listed` and `window` and nothing else, in one
/// transaction. Uncommitted: the caller seals.
pub async fn replace(pool: &SqlitePool, listed: &Listed, window: &Window) -> Result<()> {
    let mut tx = pool.begin().await.context("begin the occurrences write")?;
    for table in ["occurrences", "unlisted", "occurrences_window"] {
        // Safe: the table names are constants.
        sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM {table}")))
            .execute(&mut *tx)
            .await
            .with_context(|| format!("empty {table}"))?;
    }
    for chunk in listed.rows.chunks(CHUNK) {
        let sql = format!(
            "INSERT INTO occurrences ({}) VALUES {}",
            COLUMNS.join(", "),
            vec![ONE_ROW; chunk.len()].join(", ")
        );
        // Safe: the columns are constants, the placeholders are sized
        // from the chunk, and every value is bound.
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for r in chunk {
            q = bind_row(q, r);
        }
        q.execute(&mut *tx).await.context("write occurrences")?;
    }
    for u in &listed.unlisted {
        sqlx::query(
            "INSERT OR IGNORE INTO unlisted (event_row, calendar_id, reason) VALUES (?, ?, ?)",
        )
        .bind(&u.event_row)
        .bind(&u.calendar_id)
        .bind(&u.reason)
        .execute(&mut *tx)
        .await
        .context("write unlisted")?;
    }
    sqlx::query("INSERT INTO occurrences_window (id, from_utc, until_utc) VALUES (1, ?, ?)")
        .bind(super::utc_text(window.from))
        .bind(super::utc_text(window.until))
        .execute(&mut *tx)
        .await
        .context("write occurrences_window")?;
    tx.commit().await.context("commit the occurrences write")
}

type Query<'q> = sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments>;

fn bind_row<'q>(q: Query<'q>, r: &'q OccurrenceRow) -> Query<'q> {
    q.bind(r.id())
        .bind(&r.calendar_id)
        .bind(&r.instance_id)
        .bind(r.series_id.as_deref())
        .bind(r.original_start_time.as_deref())
        .bind(&r.start_time)
        .bind(r.end_time.as_deref())
        .bind(&r.start_utc)
        .bind(&r.end_utc)
        .bind(r.all_day)
        .bind(r.time_zone.as_deref())
        .bind(&r.status)
        .bind(r.title.as_deref())
        .bind(&r.event_row)
        .bind(&r.payload)
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_rows_placeholders_are_one_per_column() {
        assert_eq!(super::ONE_ROW.matches('?').count(), super::COLUMNS.len());
    }
}
