//! Read the whole YoLink raw store into memory for the renderer.

use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl_render::inputs::{Input, RawRange};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use datalib_etl_yolink::ingest::db_path_for;

pub use datalib_etl_timeseries_render::series::Series;

/// One row of `yolink_devices`, plus its observed extent.
#[derive(Debug, Clone)]
pub struct DeviceRow {
    /// `yolink_devices.id` — the config-chosen name, stable across runs.
    pub name: String,
    /// `temperature_humidity` | `watermeter`.
    pub kind: String,
    /// Earliest timepoint the fetcher will ever walk back to.
    pub start_ms: i64,
    /// High-water mark from the last successful fetch; `None` before the
    /// first window landed a reading.
    pub last_ts_ms: Option<i64>,
    /// SECRET — half of the per-device signed-URL credential pair. Never
    /// render it, never log it. Kept on the struct so a future consumer
    /// that legitimately needs it (a re-fetch, say) doesn't have to
    /// re-open the store, and so the omission from the document is a
    /// visible decision rather than an accident of the query.
    pub family_device_id: String,
}

/// Everything the single rendered document is built from.
#[derive(Debug, Clone)]
pub struct ParsedYolink {
    /// The commit everything was read at; `None` when nothing is
    /// committed yet, so the cursor stays unwritten.
    pub head: Option<String>,
    pub devices: Vec<DeviceRow>,
    /// Sorted by (device, metric) so the document and the plot legends
    /// are stable run to run.
    pub series: Vec<Series>,
    /// `sync_scope_config` rows: what the download step was configured
    /// to fetch, as of `updated_at`.
    pub scope_config: Vec<ScopeConfigRow>,
    /// Reading rows whose last fetch attempt recorded an error.
    pub reading_errors: i64,
    /// Total rows in `yolink_readings` (equals the summed series
    /// lengths; kept separately so a mismatch is detectable).
    pub reading_count: i64,
}

#[derive(Debug, Clone)]
pub struct ScopeConfigRow {
    pub scope: String,
    pub config: String,
    pub updated_at: String,
}

/// The tables the page reads, whole: any row of any of them moving
/// re-renders it.
pub fn inputs() -> Vec<Input> {
    [
        "yolink_devices",
        "yolink_readings",
        "yolink_readings_bookkeeping",
        "sync_scope_config",
    ]
    .into_iter()
    .map(Input::whole_table)
    .collect()
}

pub fn parse(raw_path: &Path, range: RawRange<'_>) -> Result<ParsedYolink> {
    let db_path = db_path_for(raw_path);
    if !db_path.exists() {
        anyhow::bail!(
            "yolink raw store not found at {} — run the download step first",
            db_path.display()
        );
    }
    // The render phase is driven by `futures`' executor, which enters no
    // tokio context of its own; `block_in_place` + `block_on` is exactly
    // the same shape every other provider's parse uses.
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            // Pinned at open, at the driver's commit else HEAD. No commit
            // means nothing has been committed here to render.
            let Some(reader) = datalib_etl::doltlite_raw::open_reader(&db_path, range.pin)
                .await
                .with_context(|| {
                    format!("open yolink doltlite for render {}", db_path.display())
                })?
            else {
                return Ok(ParsedYolink {
                    head: None,
                    devices: Vec::new(),
                    series: Vec::new(),
                    scope_config: Vec::new(),
                    reading_errors: 0,
                    reading_count: 0,
                });
            };
            let parsed = parse_pinned(reader.pool(), reader.pin()).await;
            reader.close().await;
            parsed
        })
    })
}

async fn parse_pinned(pool: &SqlitePool, pin: &datalib_etl::pin::Pin) -> Result<ParsedYolink> {
    let devices = load_devices(pool).await?;
    let series = load_series(pool).await?;
    let scope_config = load_scope_config(pool).await;
    let reading_errors: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pinned_yolink_readings_bookkeeping yolink_readings_bookkeeping WHERE last_error IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .unwrap_or(0);
    let reading_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pinned_yolink_readings yolink_readings")
            .fetch_one(pool)
            .await
            .context("count yolink_readings")?;

    Ok(ParsedYolink {
        head: Some(pin.commit().to_string()),
        devices,
        series,
        scope_config,
        reading_errors,
        reading_count,
    })
}

async fn load_devices(pool: &SqlitePool) -> Result<Vec<DeviceRow>> {
    let rows = sqlx::query(
        "SELECT id, kind, start_ms, last_ts_ms, family_device_id \
           FROM pinned_yolink_devices yolink_devices ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .context("load yolink_devices")?;
    Ok(rows
        .into_iter()
        .map(|r| DeviceRow {
            name: r.get::<String, _>("id"),
            kind: r.get::<String, _>("kind"),
            start_ms: r.get::<i64, _>("start_ms"),
            last_ts_ms: r.get::<Option<i64>, _>("last_ts_ms"),
            family_device_id: r.get::<String, _>("family_device_id"),
        })
        .collect())
}

/// One pass over `yolink_readings`, ordered so each (device, metric)
/// run is contiguous and ascending in time — the exact order both the
/// plot traces and the per-device stats want, so neither has to sort.
/// The `yolink_readings_by_device_ts` index covers the leading two
/// columns of the ORDER BY.
async fn load_series(pool: &SqlitePool) -> Result<Vec<Series>> {
    let rows = sqlx::query(
        "SELECT device_name, metric, ts_ms, value \
           FROM pinned_yolink_readings yolink_readings ORDER BY device_name, metric, ts_ms",
    )
    .fetch_all(pool)
    .await
    .context("load yolink_readings")?;

    let mut out: Vec<Series> = Vec::new();
    for r in rows {
        let device: String = r.get("device_name");
        let metric: String = r.get("metric");
        let ts_ms: i64 = r.get("ts_ms");
        let value: f64 = r.get("value");
        match out.last_mut() {
            Some(s) if s.device == device && s.metric == metric => s.push(ts_ms, value),
            _ => {
                let mut s = Series::new(device, metric);
                s.push(ts_ms, value);
                out.push(s);
            }
        }
    }
    Ok(out)
}

async fn load_scope_config(pool: &SqlitePool) -> Vec<ScopeConfigRow> {
    let Ok(rows) =
        sqlx::query("SELECT scope, config, updated_at FROM pinned_sync_scope_config sync_scope_config ORDER BY scope")
            .fetch_all(pool)
            .await
    else {
        return Vec::new();
    };
    rows.into_iter()
        .map(|r| ScopeConfigRow {
            scope: r.get::<String, _>("scope"),
            config: r.get::<String, _>("config"),
            updated_at: r.get::<String, _>("updated_at"),
        })
        .collect()
}
