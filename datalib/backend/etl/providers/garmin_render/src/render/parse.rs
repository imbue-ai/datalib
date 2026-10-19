//! Read what the pages need out of the Garmin raw store.

use std::path::Path;

use anyhow::{Context, Result};
use datalib_etl_render::inputs::{Input, RawRange};
use serde_json::Value;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use datalib_etl_garmin::ingest::db_path_for;

use super::daily::{pick_readiness, DayValue, Days, Extract, Show, FIELDS};

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

/// One activity: the listing's record, the detail's (JSON `null` when
/// Garmin had none), and whether its original FIT file is in the CAS.
#[derive(Debug, Clone)]
pub struct Activity {
    pub id: String,
    pub listing: Value,
    pub detail: Value,
    /// The FIT file's key in the blob store, when one is held.
    pub fit_blake3: Option<String>,
}

/// One row of an account listing (`garmin_items`).
#[derive(Debug, Clone)]
pub struct Item {
    pub id: String,
    pub kind: String,
    pub upstream_id: String,
    pub payload: Value,
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
    /// Oldest first.
    pub activities: Vec<Activity>,
    pub activity_files: i64,
    pub items: Vec<Item>,
    pub days: Days,
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
        "garmin_activity_details",
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
    let activities = load_activities(pool).await?;
    let activity_files = scalar(
        pool,
        "SELECT COUNT(*) FROM garmin_activity_files WHERE blake3 IS NOT NULL",
    )
    .await?;
    let items = load_items(pool).await?;
    let days = load_days(pool).await?;
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
        days,
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
        "SELECT json(payload) AS payload FROM garmin_account \
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
           FROM garmin_weigh_ins \
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
           FROM garmin_devices ORDER BY id",
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
           FROM garmin_daily GROUP BY metric ORDER BY metric",
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

async fn load_activities(pool: &SqlitePool) -> Result<Vec<Activity>> {
    let rows = sqlx::query(
        "SELECT a.id, json(a.payload) AS listing, json(d.payload) AS detail, \
                (SELECT f.blake3 FROM garmin_activity_files f \
                  WHERE f.activity_id = a.id AND f.blake3 IS NOT NULL \
                  ORDER BY f.id LIMIT 1) AS fit \
           FROM garmin_activities a LEFT JOIN garmin_activity_details d ON d.id = a.id \
          ORDER BY a.start_time_gmt, a.id",
    )
    .fetch_all(pool)
    .await
    .context("load garmin_activities")?;
    Ok(rows
        .into_iter()
        .map(|r| Activity {
            id: r.get("id"),
            listing: json_column(&r, "listing"),
            detail: json_column(&r, "detail"),
            fit_blake3: r.get("fit"),
        })
        .collect())
}

async fn load_items(pool: &SqlitePool) -> Result<Vec<Item>> {
    let rows = sqlx::query(
        "SELECT id, kind, upstream_id, json(payload) AS payload \
           FROM garmin_items ORDER BY kind, id",
    )
    .fetch_all(pool)
    .await
    .context("load garmin_items")?;
    Ok(rows
        .into_iter()
        .map(|r| Item {
            id: r.get("id"),
            kind: r.get("kind"),
            upstream_id: r.get("upstream_id"),
            payload: json_column(&r, "payload"),
        })
        .collect())
}

fn json_column(r: &sqlx::sqlite::SqliteRow, name: &str) -> Value {
    r.try_get::<Option<String>, _>(name)
        .ok()
        .flatten()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(Value::Null)
}

/// Every value in [`FIELDS`], by day. A path is extracted in SQL, so a
/// day's minute-by-minute arrays are never read into memory.
async fn load_days(pool: &SqlitePool) -> Result<Days> {
    let mut days = Days::new();
    let mut put = |date: String, key: &'static str, v: DayValue| {
        days.entry(date).or_default().values.insert(key, v);
    };
    let mut readiness: Option<Vec<(String, Value)>> = None;
    for f in FIELDS {
        match f.extract {
            Extract::Path(path) if f.show == Show::Text => {
                let rows: Vec<(String, Option<String>)> = sqlx::query_as(
                    "SELECT calendar_date, CAST(json_extract(json(payload), ?) AS TEXT) \
                       FROM garmin_daily WHERE metric = ?",
                )
                .bind(path)
                .bind(f.metric)
                .fetch_all(pool)
                .await
                .with_context(|| format!("load garmin_daily {} {path}", f.metric))?;
                for (date, v) in rows {
                    if let Some(v) = v.filter(|v| !v.is_empty()) {
                        put(date, f.key, DayValue::Text(v));
                    }
                }
            }
            Extract::Path(path) => {
                let rows: Vec<(String, Option<f64>)> = sqlx::query_as(
                    "SELECT calendar_date, CAST(json_extract(json(payload), ?) AS REAL) \
                       FROM garmin_daily WHERE metric = ?",
                )
                .bind(path)
                .bind(f.metric)
                .fetch_all(pool)
                .await
                .with_context(|| format!("load garmin_daily {} {path}", f.metric))?;
                for (date, v) in rows {
                    if let Some(v) = v {
                        put(date, f.key, DayValue::Num(v));
                    }
                }
            }
            Extract::Readiness(key) => {
                if readiness.is_none() {
                    let rows: Vec<(String, Option<String>)> = sqlx::query_as(
                        "SELECT calendar_date, json(payload) FROM garmin_daily \
                          WHERE metric = 'training_readiness'",
                    )
                    .fetch_all(pool)
                    .await
                    .context("load garmin_daily training_readiness")?;
                    readiness = Some(
                        rows.into_iter()
                            .filter_map(|(d, p)| Some((d, serde_json::from_str(&p?).ok()?)))
                            .collect(),
                    );
                }
                for (date, payload) in readiness.iter().flatten() {
                    match pick_readiness(payload, key) {
                        Some(Value::Number(n)) => {
                            if let Some(x) = n.as_f64() {
                                put(date.clone(), f.key, DayValue::Num(x));
                            }
                        }
                        Some(Value::String(s)) => {
                            put(date.clone(), f.key, DayValue::Text(s.clone()))
                        }
                        _ => {}
                    }
                }
            }
        }
    }
    let stamps: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT calendar_date, \
                json_extract(json(payload), '$.wellnessStartTimeGmt'), \
                json_extract(json(payload), '$.wellnessStartTimeLocal') \
           FROM garmin_daily WHERE metric = 'daily_summary'",
    )
    .fetch_all(pool)
    .await
    .context("load garmin_daily offsets")?;
    for (date, gmt, local) in stamps {
        if let (Some(day), Some(gmt), Some(local)) = (days.get_mut(&date), gmt, local) {
            day.offset_secs = super::stamps::offset_secs(&gmt, &local);
        }
    }
    Ok(days)
}

impl ParsedGarmin {
    pub fn latest_weigh_in(&self) -> Option<&WeighIn> {
        self.weigh_ins.last()
    }

    /// The page's item count: every day of a metric Garmin had data
    /// for, every weigh-in and every activity. A day Garmin answered
    /// with nothing is a row, but not a measurement.
    pub fn measurements(&self) -> i64 {
        let days: i64 = self.metrics.iter().map(|m| m.days_with_data).sum();
        days + self.weigh_ins.len() as i64 + self.activities.len() as i64
    }
}
