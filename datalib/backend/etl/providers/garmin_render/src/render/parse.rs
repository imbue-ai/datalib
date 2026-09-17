//! Read what the weight page needs out of the Garmin raw store.

use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl_render::inputs::{Input, RawRange};
use serde_json::Value;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use datalib_etl_garmin::ingest::db_path_for;

/// One weigh-in, ascending by `timestamp_gmt` in [`ParsedGarmin::weigh_ins`].
#[derive(Debug, Clone)]
pub struct WeighIn {
    pub id: String,
    pub calendar_date: String,
    pub timestamp_gmt_ms: i64,
    pub weight_kg: f64,
    pub bmi: Option<f64>,
    pub body_fat_pct: Option<f64>,
    pub source_type: Option<String>,
}

#[derive(Debug, Clone)]
pub struct Device {
    pub id: String,
    pub name: String,
    pub last_sync: Option<String>,
}

/// `(metric, rows, rows with a payload)` for one per-day metric.
#[derive(Debug, Clone)]
pub struct MetricCount {
    pub metric: String,
    pub days: i64,
    pub days_with_data: i64,
}

#[derive(Debug, Clone, Default)]
pub struct ParsedGarmin {
    /// The commit everything was read at; `None` when nothing is
    /// committed yet, so the cursor stays unwritten.
    pub head: Option<String>,
    pub display_name: Option<String>,
    pub full_name: Option<String>,
    pub weigh_ins: Vec<WeighIn>,
    pub devices: Vec<Device>,
    pub metrics: Vec<MetricCount>,
    pub activities: i64,
    pub activity_files: i64,
    pub items: i64,
}

/// The tables the page reads, whole: any row of any of them moving
/// re-renders it.
pub fn inputs() -> Vec<Input> {
    [
        "garmin_account",
        "garmin_weigh_ins",
        "garmin_devices",
        "garmin_daily",
        "garmin_activities",
        "garmin_activity_files",
        "garmin_items",
    ]
    .into_iter()
    .map(Input::whole_table)
    .collect()
}

pub fn parse(raw_path: &Path, range: RawRange<'_>) -> Result<ParsedGarmin> {
    let db_path = db_path_for(raw_path);
    if !db_path.exists() {
        anyhow::bail!(
            "garmin raw store not found at {} — run the ingest step first",
            db_path.display()
        );
    }
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            // Pinned at open, at the driver's commit else HEAD. No commit
            // means nothing has been committed here to render.
            let Some(reader) = datalib_etl::doltlite_raw::open_reader(&db_path, range.pin)
                .await
                .with_context(|| {
                    format!("open garmin doltlite for render {}", db_path.display())
                })?
            else {
                return Ok(ParsedGarmin::default());
            };
            let parsed = parse_pinned(reader.pool(), reader.pin()).await;
            reader.close().await;
            parsed
        })
    })
}

async fn parse_pinned(pool: &SqlitePool, pin: &datalib_etl::pin::Pin) -> Result<ParsedGarmin> {
    let (display_name, full_name) = load_account(pool).await?;
    let weigh_ins = load_weigh_ins(pool).await?;
    let devices = load_devices(pool).await?;
    let metrics = load_metric_counts(pool).await?;
    let activities = scalar(
        pool,
        "SELECT COUNT(*) FROM pinned_garmin_activities garmin_activities",
    )
    .await?;
    let activity_files = scalar(
        pool,
        "SELECT COUNT(*) FROM pinned_garmin_activity_files garmin_activity_files WHERE blake3 IS NOT NULL",
    )
    .await?;
    let items = scalar(
        pool,
        "SELECT COUNT(*) FROM pinned_garmin_items garmin_items",
    )
    .await?;
    Ok(ParsedGarmin {
        head: Some(pin.commit().to_string()),
        display_name,
        full_name,
        weigh_ins,
        devices,
        metrics,
        activities,
        activity_files,
        items,
    })
}

async fn scalar(pool: &SqlitePool, sql: &'static str) -> Result<i64> {
    sqlx::query_scalar(sql)
        .fetch_one(pool)
        .await
        .with_context(|| format!("garmin render: {sql}"))
}

async fn load_account(pool: &SqlitePool) -> Result<(Option<String>, Option<String>)> {
    let row = sqlx::query(
        "SELECT json(payload) AS payload FROM pinned_garmin_account garmin_account \
         WHERE id = 'social_profile'",
    )
    .fetch_optional(pool)
    .await
    .context("load garmin_account")?;
    let Some(row) = row else {
        return Ok((None, None));
    };
    let payload: String = row.try_get("payload").unwrap_or_default();
    let v: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
    Ok((
        v["displayName"].as_str().map(str::to_string),
        v["fullName"].as_str().map(str::to_string),
    ))
}

async fn load_weigh_ins(pool: &SqlitePool) -> Result<Vec<WeighIn>> {
    let rows = sqlx::query(
        "SELECT id, calendar_date, timestamp_gmt, weight_g, source_type, json(payload) AS payload \
           FROM pinned_garmin_weigh_ins garmin_weigh_ins \
          WHERE weight_g IS NOT NULL AND timestamp_gmt IS NOT NULL \
          ORDER BY timestamp_gmt, id",
    )
    .fetch_all(pool)
    .await
    .context("load garmin_weigh_ins")?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let payload: String = r.try_get("payload").unwrap_or_default();
            let v: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
            WeighIn {
                id: r.get::<String, _>("id"),
                calendar_date: r
                    .get::<Option<String>, _>("calendar_date")
                    .unwrap_or_default(),
                timestamp_gmt_ms: r.get::<i64, _>("timestamp_gmt"),
                weight_kg: r.get::<f64, _>("weight_g") / 1000.0,
                bmi: v["bmi"].as_f64(),
                body_fat_pct: v["bodyFat"].as_f64(),
                source_type: r.get::<Option<String>, _>("source_type"),
            }
        })
        .collect())
}

async fn load_devices(pool: &SqlitePool) -> Result<Vec<Device>> {
    let rows = sqlx::query(
        "SELECT id, product_display_name, json(payload) AS payload \
           FROM pinned_garmin_devices garmin_devices ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .context("load garmin_devices")?;
    Ok(rows
        .into_iter()
        .map(|r| {
            let id: String = r.get("id");
            let payload: String = r.try_get("payload").unwrap_or_default();
            let v: Value = serde_json::from_str(&payload).unwrap_or(Value::Null);
            Device {
                name: r
                    .get::<Option<String>, _>("product_display_name")
                    .or_else(|| v["displayName"].as_str().map(str::to_string))
                    .unwrap_or_else(|| id.clone()),
                last_sync: v["lastSyncTimestampGMT"].as_str().map(str::to_string),
                id,
            }
        })
        .collect())
}

async fn load_metric_counts(pool: &SqlitePool) -> Result<Vec<MetricCount>> {
    let rows = sqlx::query(
        "SELECT metric, COUNT(*) AS days, \
                SUM(CASE WHEN json(payload) <> 'null' THEN 1 ELSE 0 END) AS with_data \
           FROM pinned_garmin_daily garmin_daily GROUP BY metric ORDER BY metric",
    )
    .fetch_all(pool)
    .await
    .context("count garmin_daily")?;
    Ok(rows
        .into_iter()
        .map(|r| MetricCount {
            metric: r.get::<String, _>("metric"),
            days: r.get::<i64, _>("days"),
            days_with_data: r.get::<i64, _>("with_data"),
        })
        .collect())
}

impl ParsedGarmin {
    pub fn latest_weigh_in(&self) -> Option<&WeighIn> {
        self.weigh_ins.last()
    }
}
