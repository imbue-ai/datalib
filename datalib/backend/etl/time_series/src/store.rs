//! Opening a time-series store and the statements that read and write
//! it. Both writers — a source's `render_time_series` and the
//! `time_series_index` — open their store here and carry out a
//! [`crate::plan::Plan`] with these, inside one transaction.

use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::doltlite_raw::{self, StoreKind};
use datalib_time_series_schema::{TimeKind, TimeSeriesPointRow, TimeSeriesRow};
use sqlx::{Row, SqliteConnection, SqlitePool};

pub use datalib_runtime::layout::TIME_SERIES_DB as DB;

/// Points per INSERT: each is one bound JSON array, so the statement's
/// size is bounded by this, not by SQLite's bind limit.
const POINTS_PER_INSERT: usize = 50_000;

/// Open `db_path` as its one writer, creating the tables and recording
/// `kind` in `_datalib_meta`. The schema is committed before this returns.
pub async fn open(db_path: &Path, kind: StoreKind) -> Result<SqlitePool> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let ddl = datalib_time_series_schema::ddl();
    let pool = doltlite_raw::open_derived_indexed(
        db_path,
        &ddl,
        &datalib_time_series_schema::indexes(),
        kind,
    )
    .await
    .with_context(|| format!("open the time-series store at {}", db_path.display()))?;
    let versions = datalib_store_meta::Versions {
        schema: 0,
        shared: 0,
    };
    datalib_store_meta::write(&pool, kind, &schema_hash(), versions)
        .await
        .context("write _datalib_meta for a time-series store")?;
    doltlite_raw::commit_run(&pool, "schema: time series")
        .await
        .context("commit the time-series schema")?;
    Ok(pool)
}

/// The shape `open` records in `_datalib_meta`: what a launch's migrate
/// pass compares to decide whether the store must be rebuilt.
pub fn schema_hash() -> String {
    doltlite_raw::recorded_shape(datalib_time_series_schema::ddl())
}

const ROW_COLUMNS: &str = "ts_key, time_series_id, source_id, entity, metric, label, unit, \
     time_kind, points_hash, point_count, first_t, last_t";

/// Every time series a store holds, without its points.
pub async fn rows(pool: &SqlitePool) -> Result<Vec<TimeSeriesRow>> {
    let sql = format!("SELECT {ROW_COLUMNS} FROM time_series ORDER BY ts_key");
    // Safe: the column list is a constant.
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await
        .context("read time_series")?;
    rows.iter().map(row_of).collect()
}

fn row_of(r: &sqlx::sqlite::SqliteRow) -> Result<TimeSeriesRow> {
    let kind: String = r.try_get("time_kind")?;
    Ok(TimeSeriesRow {
        ts_key: r.try_get("ts_key")?,
        time_series_id: r.try_get("time_series_id")?,
        source_id: r.try_get("source_id")?,
        entity: r.try_get("entity")?,
        metric: r.try_get("metric")?,
        label: r.try_get("label")?,
        unit: r.try_get("unit")?,
        time_kind: TimeKind::parse(&kind).with_context(|| {
            format!("a time series of kind {kind:?}, which this build does not know")
        })?,
        points_hash: r.try_get("points_hash")?,
        point_count: r.try_get("point_count")?,
        first_t: r.try_get("first_t")?,
        last_t: r.try_get("last_t")?,
    })
}

/// One time series' points, ascending by `t`.
pub async fn points(pool: &SqlitePool, ts_key: i64) -> Result<Vec<TimeSeriesPointRow>> {
    let rows = sqlx::query(
        "SELECT ts_key, t, value, text FROM time_series_points WHERE ts_key = ? ORDER BY t",
    )
    .bind(ts_key)
    .fetch_all(pool)
    .await
    .context("read time_series_points")?;
    rows.iter()
        .map(|r| {
            Ok(TimeSeriesPointRow {
                ts_key: r.try_get("ts_key")?,
                t: r.try_get("t")?,
                value: r.try_get("value")?,
                text: r.try_get("text")?,
            })
        })
        .collect()
}

pub async fn write_row(conn: &mut SqliteConnection, row: &TimeSeriesRow) -> Result<()> {
    let sql = format!(
        "INSERT OR REPLACE INTO time_series ({ROW_COLUMNS}) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
    );
    // Safe: the column list is a constant and every value is bound.
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(row.ts_key)
        .bind(&row.time_series_id)
        .bind(&row.source_id)
        .bind(&row.entity)
        .bind(&row.metric)
        .bind(&row.label)
        .bind(&row.unit)
        .bind(row.time_kind.as_str())
        .bind(&row.points_hash)
        .bind(row.point_count)
        .bind(row.first_t)
        .bind(row.last_t)
        .execute(conn)
        .await
        .with_context(|| format!("write time series {}", row.time_series_id))?;
    Ok(())
}

/// Delete `ts_key`'s points and insert `points` in their place.
pub async fn replace_points(
    conn: &mut SqliteConnection,
    ts_key: i64,
    points: &[TimeSeriesPointRow],
) -> Result<()> {
    sqlx::query("DELETE FROM time_series_points WHERE ts_key = ?")
        .bind(ts_key)
        .execute(&mut *conn)
        .await
        .context("delete a time series' points")?;
    for chunk in points.chunks(POINTS_PER_INSERT) {
        let json = serde_json::to_string(
            &chunk
                .iter()
                .map(|p| (p.t, p.value, p.text.as_deref()))
                .collect::<Vec<_>>(),
        )?;
        sqlx::query(
            "INSERT INTO time_series_points (ts_key, t, value, text) \
             SELECT ?, json_extract(value, '$[0]'), json_extract(value, '$[1]'), \
                    json_extract(value, '$[2]') \
             FROM json_each(?)",
        )
        .bind(ts_key)
        .bind(json)
        .execute(&mut *conn)
        .await
        .context("insert a time series' points")?;
    }
    Ok(())
}

pub async fn remove(conn: &mut SqliteConnection, ts_key: i64) -> Result<()> {
    sqlx::query("DELETE FROM time_series_points WHERE ts_key = ?")
        .bind(ts_key)
        .execute(&mut *conn)
        .await
        .context("delete a time series' points")?;
    sqlx::query("DELETE FROM time_series WHERE ts_key = ?")
        .bind(ts_key)
        .execute(&mut *conn)
        .await
        .context("delete a time series")?;
    Ok(())
}
