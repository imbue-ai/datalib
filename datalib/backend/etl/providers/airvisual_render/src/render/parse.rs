//! Read the whole AirVisual raw store into memory for the renderer.
//! The store is wide (one column per measurement); the plots want one
//! series per (device, measurement), so this is where the pivot happens.

use std::collections::{BTreeMap, HashMap};
use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl_render::inputs::{Input, RawRange};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use datalib_etl_airvisual::ingest::clock::{clock_model, ClockModel};
use datalib_etl_airvisual::ingest::db_path_for;
use datalib_etl_airvisual::ingest::schema_raw::load_clock_changes;

pub use datalib_etl_timeseries_render::series::Series;

use super::units::METRICS;

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
    /// The commit everything was read at; `None` when nothing is
    /// committed yet, so the cursor stays unwritten.
    pub head: Option<String>,
    pub devices: Vec<DeviceRow>,
    /// `Series::device` is the device *id*; sorted by (device, metric)
    /// so the document and the plot legends are stable run to run.
    pub series: Vec<Series>,
    pub files: Vec<IngestedFile>,
    /// Total rows in `airvisual_samples`.
    pub sample_count: i64,
}

/// The tables the page reads, whole: any row of any of them moving
/// re-renders it.
pub fn inputs() -> Vec<Input> {
    [
        "airvisual_devices",
        "airvisual_samples",
        "airvisual_clock_changes",
        "ingested_files",
    ]
    .into_iter()
    .map(Input::whole_table)
    .collect()
}

pub fn parse(raw_path: &Path, range: RawRange<'_>) -> Result<ParsedAirvisual> {
    let db_path = db_path_for(raw_path);
    if !db_path.exists() {
        anyhow::bail!(
            "airvisual raw store not found at {} — run the ingest step first",
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
                    format!("open airvisual doltlite for render {}", db_path.display())
                })?
            else {
                return Ok(ParsedAirvisual {
                    head: None,
                    devices: Vec::new(),
                    series: Vec::new(),
                    files: Vec::new(),
                    sample_count: 0,
                });
            };
            let parsed = parse_pinned(reader.pool(), reader.pin()).await;
            reader.close().await;
            parsed
        })
    })
}

async fn parse_pinned(pool: &SqlitePool, pin: &datalib_etl::pin::Pin) -> Result<ParsedAirvisual> {
    let mut devices = load_devices(pool).await?;
    let models = load_clocks(pool, &devices).await?;
    let (series, newest) = load_series(pool, &models).await?;
    for device in &mut devices {
        device.last_ts_ms = newest.get(&device.id).copied();
    }
    let files = load_files(pool).await;
    let sample_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM airvisual_samples")
        .fetch_one(pool)
        .await
        .context("count airvisual_samples")?;

    Ok(ParsedAirvisual {
        head: Some(pin.commit().to_string()),
        devices,
        series,
        files,
        sample_count,
    })
}

async fn load_devices(pool: &SqlitePool) -> Result<Vec<DeviceRow>> {
    let rows = sqlx::query(
        "SELECT id, name, model, mac_address, app_version, system_version, timezone, last_ts_ms \
           FROM airvisual_devices ORDER BY name, id",
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

/// What each device's clock means. What is doubtful about one is the
/// download's to report, which holds the same facts.
async fn load_clocks(
    pool: &SqlitePool,
    devices: &[DeviceRow],
) -> Result<HashMap<String, ClockModel>> {
    let mut models = HashMap::new();
    for device in devices {
        let changes = load_clock_changes(pool, &device.id)
            .await
            .with_context(|| format!("load the clock changes of {}", device.id))?;
        let (model, _) = clock_model(&changes, device.timezone.as_deref());
        models.insert(device.id.clone(), model);
    }
    Ok(models)
}

/// A device's readings in real time, the same moment twice (a `corrupt_` /
/// `restored_` pair) keeping the later one.
fn in_real_order(series: &mut Series) {
    let mut points: Vec<(i64, f64)> = series
        .ts_ms
        .iter()
        .copied()
        .zip(series.values.iter().copied())
        .collect();
    points.sort_by_key(|(t, _)| *t);
    points.reverse();
    points.dedup_by_key(|(t, _)| *t);
    points.reverse();
    (series.ts_ms, series.values) = points.into_iter().unzip();
}

/// One pass over `airvisual_samples`, fanned out into one series per
/// measurement column that has a value, in real time; and each device's
/// newest reading. What the store holds is the device's own count, which
/// goes back when its clock does, so its order is not time's.
async fn load_series(
    pool: &SqlitePool,
    models: &HashMap<String, ClockModel>,
) -> Result<(Vec<Series>, HashMap<String, i64>)> {
    let columns: Vec<&'static str> = METRICS.iter().map(|m| m.metric).collect();
    // Audited: `columns` is the `&'static str` metric table above, our
    // own column names; no runtime data reaches the statement.
    let sql = format!(
        "SELECT device_id, ts_ms, source_file, {} \
           FROM airvisual_samples ORDER BY device_id, ts_ms",
        columns.join(", ")
    );
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await
        .context("load airvisual_samples")?;

    let mut by_key: BTreeMap<(String, &'static str), Series> = BTreeMap::new();
    let mut newest: HashMap<String, i64> = HashMap::new();
    for r in rows {
        let device: String = r.get("device_id");
        let wall_ms: i64 = r.get("ts_ms");
        let ts_ms = match models.get(&device) {
            Some(model) => model.real_ms(r.get::<&str, _>("source_file"), wall_ms),
            None => wall_ms,
        };
        let held = newest.entry(device.clone()).or_insert(ts_ms);
        *held = (*held).max(ts_ms);
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
    let mut series: Vec<Series> = by_key.into_values().collect();
    series.iter_mut().for_each(in_real_order);
    Ok((series, newest))
}

async fn load_files(pool: &SqlitePool) -> Vec<IngestedFile> {
    let Ok(rows) = sqlx::query("SELECT rel_path, size_bytes FROM ingested_files ORDER BY rel_path")
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The clocks going back put an hour's readings after the hour they
    /// follow in the device's own count, and a pair of files can say one
    /// moment twice.
    #[test]
    fn readings_are_put_in_real_order_and_a_repeat_keeps_the_later() {
        let mut series = Series::new("d".into(), "m".into());
        for (t, v) in [(30, 3.0), (10, 1.0), (20, 2.0), (10, 9.0)] {
            series.push(t, v);
        }
        in_real_order(&mut series);
        assert_eq!(series.ts_ms, [10, 20, 30]);
        assert_eq!(series.values, [9.0, 2.0, 3.0]);
    }
}
