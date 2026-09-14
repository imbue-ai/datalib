//! Read the whole AirVisual raw store into memory for the renderer, and
//! decide up front whether there is anything to do. The store is wide
//! (one column per measurement); the plots want one series per
//! (device, measurement), so this is where the pivot happens.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use datalib_etl_airvisual::ingest::db_path_for;

pub use datalib_etl_timeseries_render::series::Series;

use super::units::METRICS;

pub enum Parsed {
    /// The store's HEAD matches the render cursor: the single rendered
    /// page is already current.
    UpToDate { head: String },
    /// The store moved (or there was no usable cursor). Everything the
    /// document needs, loaded.
    Fresh(Box<ParsedAirvisual>),
}

/// One row of `airvisual_devices`: the serial that keys the samples,
/// and the name a person knows it by.
#[derive(Debug, Clone)]
pub struct DeviceRow {
    pub id: String,
    pub name: String,
    pub model: Option<String>,
    pub mac_address: Option<String>,
    pub app_version: Option<String>,
    pub system_version: Option<String>,
    pub timezone: Option<String>,
    pub last_ts_ms: Option<i64>,
}

/// One row of `ingested_files`: a history file the ingest step has
/// finished with.
#[derive(Debug, Clone)]
pub struct IngestedFile {
    pub rel_path: String,
    pub size_bytes: i64,
}

/// Everything the single rendered document is built from.
#[derive(Debug, Clone)]
pub struct ParsedAirvisual {
    /// HEAD at scan time, to stamp into the cursor after a successful
    /// render. `None` when `dolt_log()` is unavailable — then the cursor
    /// stays unwritten and the next run re-renders.
    pub head: Option<String>,
    pub scan_elapsed: Option<Duration>,
    pub devices: Vec<DeviceRow>,
    /// `Series::device` is the device *id*; sorted by (device, metric)
    /// so the document and the plot legends are stable run to run.
    pub series: Vec<Series>,
    pub files: Vec<IngestedFile>,
    /// Total rows in `airvisual_samples`.
    pub sample_count: i64,
}

pub fn parse(raw_path: &Path, last_render_hash: Option<&str>) -> Result<Parsed> {
    let db_path = db_path_for(raw_path);
    if !db_path.exists() {
        anyhow::bail!(
            "airvisual raw store not found at {} — run the ingest step first",
            db_path.display()
        );
    }
    // The render phase is driven by `futures`' executor, which enters no
    // tokio context of its own; `block_in_place` + the ambient handle is
    // the same shape every other provider's parse uses.
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current()
            .block_on(async move { parse_async(&db_path, last_render_hash).await })
    })
}

async fn parse_async(db_path: &Path, last_render_hash: Option<&str>) -> Result<Parsed> {
    let pool = datalib_etl::doltlite_raw::open_reader(db_path)
        .await
        .with_context(|| format!("open airvisual doltlite for render {}", db_path.display()))?;

    let started = std::time::Instant::now();
    let pin = datalib_etl::pin::head(&pool).await?;
    let scan_elapsed = Some(started.elapsed());
    let Some(pin) = pin else {
        return Ok(Parsed::Fresh(Box::new(ParsedAirvisual {
            head: None,
            scan_elapsed,
            devices: Vec::new(),
            series: Vec::new(),
            files: Vec::new(),
            sample_count: 0,
        })));
    };
    datalib_etl::pin::install_views(&pool, &pin)
        .await
        .context("pin the airvisual raw store for render")?;
    let head = Some(pin.commit().to_string());

    if let (Some(head), Some(last)) = (head.as_deref(), last_render_hash) {
        if head == last {
            return Ok(Parsed::UpToDate {
                head: head.to_string(),
            });
        }
    }

    let devices = load_devices(&pool).await?;
    let series = load_series(&pool).await?;
    let files = load_files(&pool).await;
    let sample_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM pinned_airvisual_samples airvisual_samples")
            .fetch_one(&pool)
            .await
            .context("count airvisual_samples")?;

    Ok(Parsed::Fresh(Box::new(ParsedAirvisual {
        head,
        scan_elapsed,
        devices,
        series,
        files,
        sample_count,
    })))
}

async fn load_devices(pool: &SqlitePool) -> Result<Vec<DeviceRow>> {
    let rows = sqlx::query(
        "SELECT id, name, model, mac_address, app_version, system_version, timezone, last_ts_ms \
           FROM pinned_airvisual_devices airvisual_devices ORDER BY name, id",
    )
    .fetch_all(pool)
    .await
    .context("load airvisual_devices")?;
    Ok(rows
        .into_iter()
        .map(|r| DeviceRow {
            id: r.get::<String, _>("id"),
            name: r.get::<String, _>("name"),
            model: r.get::<Option<String>, _>("model"),
            mac_address: r.get::<Option<String>, _>("mac_address"),
            app_version: r.get::<Option<String>, _>("app_version"),
            system_version: r.get::<Option<String>, _>("system_version"),
            timezone: r.get::<Option<String>, _>("timezone"),
            last_ts_ms: r.get::<Option<i64>, _>("last_ts_ms"),
        })
        .collect())
}

/// One pass over `airvisual_samples` in (device, time) order, fanned
/// out into one series per measurement column that has a value. The
/// `airvisual_samples_by_device_ts` index covers the ORDER BY.
async fn load_series(pool: &SqlitePool) -> Result<Vec<Series>> {
    let columns: Vec<&'static str> = METRICS.iter().map(|m| m.metric).collect();
    // Audited: `columns` is the `&'static str` metric table above, our
    // own column names; no runtime data reaches the statement.
    let sql = format!(
        "SELECT device_id, ts_ms, {} \
           FROM pinned_airvisual_samples airvisual_samples ORDER BY device_id, ts_ms",
        columns.join(", ")
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await
        .context("load airvisual_samples")?;

    let mut by_key: BTreeMap<(String, &'static str), Series> = BTreeMap::new();
    for r in rows {
        let device: String = r.get("device_id");
        let ts_ms: i64 = r.get("ts_ms");
        for col in &columns {
            let Some(value) = r.get::<Option<f64>, _>(*col) else {
                continue;
            };
            by_key
                .entry((device.clone(), col))
                .or_insert_with(|| Series::new(device.clone(), col.to_string()))
                .push(ts_ms, value);
        }
    }
    Ok(by_key.into_values().collect())
}

async fn load_files(pool: &SqlitePool) -> Vec<IngestedFile> {
    let Ok(rows) = sqlx::query(
        "SELECT rel_path, size_bytes FROM pinned_ingested_files ingested_files ORDER BY rel_path",
    )
    .fetch_all(pool)
    .await
    else {
        return Vec::new();
    };
    rows.into_iter()
        .map(|r| IngestedFile {
            rel_path: r.get::<String, _>("rel_path"),
            size_bytes: r.get::<i64, _>("size_bytes"),
        })
        .collect()
}

impl ParsedAirvisual {
    /// A device's display name, or its id when no device row names it.
    pub fn device_label<'a>(&'a self, id: &'a str) -> &'a str {
        self.devices
            .iter()
            .find(|d| d.id == id)
            .map(|d| d.name.as_str())
            .unwrap_or(id)
    }

    pub fn series_by_device(&self) -> BTreeMap<&str, Vec<&Series>> {
        datalib_etl_timeseries_render::series::by_device(&self.series)
    }

    pub fn latest_ts_ms(&self) -> Option<i64> {
        datalib_etl_timeseries_render::series::latest_ts_ms(&self.series)
    }

    pub fn earliest_ts_ms(&self) -> Option<i64> {
        datalib_etl_timeseries_render::series::earliest_ts_ms(&self.series)
    }
}
