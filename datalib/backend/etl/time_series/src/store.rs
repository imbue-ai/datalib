//! Opening a time-series store and the statements that read and write
//! it. A source's store (`<group>/render_time_series`) holds its rows
//! and their chunks; the index (`unified_time_series_index/
//! time_series_index`) holds every source's rows and where each source's
//! store is, and no points.

use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl::doltlite_raw::{self, StoreKind};
use datalib_time_series_schema::{
    TimeKind, TimeSeriesChunkRow, TimeSeriesRow, TimeSeriesSourceRow, ValueType,
};
use sqlx::{Row, SqliteConnection, SqlitePool};

pub use datalib_runtime::layout::TIME_SERIES_DB as DB;

fn ddl(kind: StoreKind) -> Vec<&'static str> {
    match kind {
        StoreKind::TimeSeriesIndex => datalib_time_series_schema::index_ddl(),
        _ => datalib_time_series_schema::store_ddl(),
    }
}

/// Open `db_path` as its one writer, creating the tables `kind` has and
/// recording `kind` in `_datalib_meta`. A store in an older shape is
/// rebuilt empty; the schema is committed before this returns.
pub async fn open(db_path: &Path, kind: StoreKind) -> Result<SqlitePool> {
    if let Some(parent) = db_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let pool = doltlite_raw::open_derived_indexed(
        db_path,
        &ddl(kind),
        &datalib_time_series_schema::indexes(),
        kind,
    )
    .await
    .with_context(|| format!("open the time-series store at {}", db_path.display()))?;
    // The table points were kept in, one row each, before they were
    // chunks; a rebuild drops only the tables the shape still has.
    sqlx::query("DROP TABLE IF EXISTS time_series_points")
        .execute(&pool)
        .await
        .context("drop the retired time_series_points")?;
    let versions = datalib_store_meta::Versions {
        schema: 0,
        shared: 0,
    };
    datalib_store_meta::write(&pool, kind, &schema_hash(kind), versions)
        .await
        .context("write _datalib_meta for a time-series store")?;
    doltlite_raw::commit_run(&pool, "schema: time series")
        .await
        .context("commit the time-series schema")?;
    Ok(pool)
}

/// The shape `open` records in `_datalib_meta`: what a launch's migrate
/// pass compares to decide whether the store must be rebuilt.
pub fn schema_hash(kind: StoreKind) -> String {
    doltlite_raw::recorded_shape(ddl(kind))
}

const ROW_COLUMNS: &str = "source_id, ts_key, time_series_id, about_uuid, name, human_name, \
     unit, value_type, time_kind, point_count, first_t, last_t";

/// Every time series a store holds, without its points.
pub async fn rows(pool: &SqlitePool) -> Result<Vec<TimeSeriesRow>> {
    let sql = format!("SELECT {ROW_COLUMNS} FROM time_series ORDER BY source_id, ts_key");
    // Safe: the column list is a constant.
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await
        .context("read time_series")?;
    rows.iter().map(row_of).collect()
}

fn row_of(r: &sqlx::sqlite::SqliteRow) -> Result<TimeSeriesRow> {
    let time_kind: String = r.try_get("time_kind")?;
    let value_type: String = r.try_get("value_type")?;
    Ok(TimeSeriesRow {
        source_id: r.try_get("source_id")?,
        ts_key: r.try_get("ts_key")?,
        time_series_id: r.try_get("time_series_id")?,
        about_uuid: r.try_get("about_uuid")?,
        name: r.try_get("name")?,
        human_name: r.try_get("human_name")?,
        unit: r.try_get("unit")?,
        value_type: ValueType::parse(&value_type).with_context(|| {
            format!("a time series of values {value_type:?}, which this build does not know")
        })?,
        time_kind: TimeKind::parse(&time_kind).with_context(|| {
            format!("a time series of kind {time_kind:?}, which this build does not know")
        })?,
        point_count: r.try_get("point_count")?,
        first_t: r.try_get("first_t")?,
        last_t: r.try_get("last_t")?,
    })
}

pub async fn write_row(conn: &mut SqliteConnection, row: &TimeSeriesRow) -> Result<()> {
    let sql = format!(
        "INSERT OR REPLACE INTO time_series ({ROW_COLUMNS}) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)"
    );
    // Safe: the column list is a constant and every value is bound.
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(&row.source_id)
        .bind(row.ts_key)
        .bind(&row.time_series_id)
        .bind(&row.about_uuid)
        .bind(&row.name)
        .bind(&row.human_name)
        .bind(&row.unit)
        .bind(row.value_type.as_str())
        .bind(row.time_kind.as_str())
        .bind(row.point_count)
        .bind(row.first_t)
        .bind(row.last_t)
        .execute(conn)
        .await
        .with_context(|| format!("write time series {}", row.time_series_id))?;
    Ok(())
}

pub async fn remove_row(conn: &mut SqliteConnection, source_id: &str, ts_key: i64) -> Result<()> {
    sqlx::query("DELETE FROM time_series WHERE source_id = ? AND ts_key = ?")
        .bind(source_id)
        .bind(ts_key)
        .execute(conn)
        .await
        .context("delete a time series")?;
    Ok(())
}

const CHUNK_COLUMNS: &str = "ts_key, first_t, last_t, n, min, max, sum, encoding, points";

fn chunk_of(r: &sqlx::sqlite::SqliteRow) -> Result<TimeSeriesChunkRow> {
    Ok(TimeSeriesChunkRow {
        ts_key: r.try_get("ts_key")?,
        first_t: r.try_get("first_t")?,
        last_t: r.try_get("last_t")?,
        n: r.try_get("n")?,
        min: r.try_get("min")?,
        max: r.try_get("max")?,
        sum: r.try_get("sum")?,
        encoding: r.try_get("encoding")?,
        points: r.try_get("points")?,
    })
}

/// `ts_key`'s chunks that hold a point in `[from, to]`, by time.
pub async fn chunks(
    pool: &SqlitePool,
    ts_key: i64,
    from: i64,
    to: i64,
) -> Result<Vec<TimeSeriesChunkRow>> {
    let sql = format!(
        "SELECT {CHUNK_COLUMNS} FROM time_series_chunks \
         WHERE ts_key = ? AND first_t <= ? AND last_t >= ? ORDER BY first_t"
    );
    // Safe: the column list is a constant and every value is bound.
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(ts_key)
        .bind(to)
        .bind(from)
        .fetch_all(pool)
        .await
        .context("read a time series' chunks")?;
    rows.iter().map(chunk_of).collect()
}

/// Every chunk of `ts_key`.
pub async fn all_chunks(pool: &SqlitePool, ts_key: i64) -> Result<Vec<TimeSeriesChunkRow>> {
    chunks(pool, ts_key, i64::MIN, i64::MAX).await
}

pub async fn write_chunk(conn: &mut SqliteConnection, c: &TimeSeriesChunkRow) -> Result<()> {
    let sql = format!(
        "INSERT OR REPLACE INTO time_series_chunks ({CHUNK_COLUMNS}) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)"
    );
    // Safe: the column list is a constant and every value is bound.
    sqlx::query(sqlx::AssertSqlSafe(sql))
        .bind(c.ts_key)
        .bind(c.first_t)
        .bind(c.last_t)
        .bind(c.n)
        .bind(c.min)
        .bind(c.max)
        .bind(c.sum)
        .bind(c.encoding)
        .bind(&c.points)
        .execute(conn)
        .await
        .context("write a time series' chunk")?;
    Ok(())
}

pub async fn remove_chunk(conn: &mut SqliteConnection, ts_key: i64, first_t: i64) -> Result<()> {
    sqlx::query("DELETE FROM time_series_chunks WHERE ts_key = ? AND first_t = ?")
        .bind(ts_key)
        .bind(first_t)
        .execute(conn)
        .await
        .context("delete a time series' chunk")?;
    Ok(())
}

pub async fn remove_chunks(conn: &mut SqliteConnection, ts_key: i64) -> Result<()> {
    sqlx::query("DELETE FROM time_series_chunks WHERE ts_key = ?")
        .bind(ts_key)
        .execute(conn)
        .await
        .context("delete a time series' chunks")?;
    Ok(())
}

/// The index's record of every source store it read.
pub async fn sources(pool: &SqlitePool) -> Result<Vec<TimeSeriesSourceRow>> {
    let rows = sqlx::query(
        "SELECT source_id, store, commit_hash FROM time_series_sources ORDER BY source_id",
    )
    .fetch_all(pool)
    .await
    .context("read time_series_sources")?;
    rows.iter()
        .map(|r| {
            Ok(TimeSeriesSourceRow {
                source_id: r.try_get("source_id")?,
                store: r.try_get("store")?,
                commit_hash: r.try_get("commit_hash")?,
            })
        })
        .collect()
}

pub async fn write_source(conn: &mut SqliteConnection, s: &TimeSeriesSourceRow) -> Result<()> {
    sqlx::query(
        "INSERT OR REPLACE INTO time_series_sources (source_id, store, commit_hash) \
         VALUES (?, ?, ?)",
    )
    .bind(&s.source_id)
    .bind(&s.store)
    .bind(&s.commit_hash)
    .execute(conn)
    .await
    .with_context(|| format!("record where {}'s time series are", s.source_id))?;
    Ok(())
}

/// Forget a source: its row in `time_series_sources` and its rows.
pub async fn remove_source(conn: &mut SqliteConnection, source_id: &str) -> Result<()> {
    sqlx::query("DELETE FROM time_series WHERE source_id = ?")
        .bind(source_id)
        .execute(&mut *conn)
        .await
        .context("delete a source's time series")?;
    sqlx::query("DELETE FROM time_series_sources WHERE source_id = ?")
        .bind(source_id)
        .execute(&mut *conn)
        .await
        .context("delete a source from the index")?;
    Ok(())
}
