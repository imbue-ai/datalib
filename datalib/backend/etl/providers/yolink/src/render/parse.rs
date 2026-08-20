//! Read the whole YoLink raw store into memory for the renderer, and
//! decide up front whether there is anything to do.
//!
//! ## The cursor is the store's HEAD, and that is the whole story
//!
//! Providers that render one document per conversation run a
//! `dolt_diff_<table>` scan (see
//! [`datalib_etl::doltlite_raw::scan_buckets`]) to find *which* buckets
//! changed. YoLink renders one document for the entire store, so a
//! per-bucket answer has nothing to narrow: either the store moved and
//! the single page is stale, or it didn't and the page is current.
//!
//! So [`parse`] asks for the HEAD commit hash — one `dolt_log()` row —
//! and compares it against `_render_cursor.json`. On a match it returns
//! [`Parsed::UpToDate`] having touched zero reading rows; the ~150k-row
//! `SELECT` below only ever runs when a download actually appended
//! something. That is exactly the "no new data, no re-render" cursor,
//! and it costs one query to evaluate.
//!
//! `RENDER_VERSION` rides along in the cursor's `params` (see
//! [`crate::render::render::cursor_params`]), so bumping it invalidates
//! the fast path too — otherwise a renderer change would only reach
//! mirrors that happened to sync new readings.
//!
//! ## Secrets
//!
//! `yolink_devices.family_device_id` is half of the per-device
//! signed-URL secret pair (see `download/schema_raw.rs`): anyone holding
//! it plus the device UDID can pull that device's entire history,
//! forever. It is read here only so [`DeviceRow`] mirrors the table
//! faithfully; [`crate::render::render`] must never put it in the
//! rendered document. See [`DeviceRow::family_device_id`].

use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

use crate::download::db_path_for;

/// Outcome of a parse attempt.
pub enum Parsed {
    /// The store's HEAD matches the render cursor: the single rendered
    /// page is already current. Carries the hash purely for logging —
    /// the cursor file already holds it, so nothing needs rewriting.
    UpToDate { head: String },
    /// The store moved (or there was no usable cursor). Everything the
    /// document needs, loaded.
    Fresh(Box<ParsedYolink>),
}

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

/// All readings for one (device, metric) pair, ascending by timestamp.
/// Values are **as stored** — conversion to SI happens in the renderer,
/// through [`crate::render::units`].
#[derive(Debug, Clone)]
pub struct Series {
    pub device: String,
    pub metric: String,
    /// Unix milliseconds, ascending.
    pub ts_ms: Vec<i64>,
    /// Raw stored values, parallel to `ts_ms`.
    pub values: Vec<f64>,
}

/// One `dolt_log()` entry — the store's own account of how it got here.
#[derive(Debug, Clone)]
pub struct CommitRow {
    pub hash: String,
    pub date: String,
    pub message: String,
}

/// Everything the single rendered document is built from.
#[derive(Debug, Clone)]
pub struct ParsedYolink {
    /// HEAD at scan time, to stamp into the cursor after a successful
    /// render. `None` when `dolt_log()` is unavailable (stock
    /// libsqlite3) — then the cursor stays unwritten and the next run
    /// re-renders, which is the safe direction.
    pub head: Option<String>,
    /// Wall-clock cost of the HEAD lookup, recorded in the cursor so the
    /// "is the scan getting slower?" question stays answerable.
    pub scan_elapsed: Option<Duration>,
    pub devices: Vec<DeviceRow>,
    /// Sorted by (device, metric) so the document and the plot legends
    /// are stable run to run.
    pub series: Vec<Series>,
    /// `dolt_log()`, newest first.
    pub commits: Vec<CommitRow>,
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

/// Open the store, check HEAD against `last_render_hash`, and load
/// everything if it moved.
pub fn parse(raw_path: &Path, last_render_hash: Option<&str>) -> Result<Parsed> {
    let db_path = db_path_for(raw_path);
    if !db_path.exists() {
        anyhow::bail!(
            "yolink raw store not found at {} — run the download step first",
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
    let opts =
        SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))?.read_only(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(60))
        .connect_with(opts)
        .await
        .with_context(|| format!("open yolink doltlite for render {}", db_path.display()))?;

    let started = std::time::Instant::now();
    let head: Option<String> =
        sqlx::query_scalar("SELECT commit_hash FROM dolt_log() ORDER BY date DESC LIMIT 1")
            .fetch_optional(&pool)
            .await
            .ok()
            .flatten();
    let scan_elapsed = Some(started.elapsed());

    if let (Some(head), Some(last)) = (head.as_deref(), last_render_hash) {
        if head == last {
            return Ok(Parsed::UpToDate {
                head: head.to_string(),
            });
        }
    }

    let devices = load_devices(&pool).await?;
    let series = load_series(&pool).await?;
    let commits = load_commits(&pool).await;
    let scope_config = load_scope_config(&pool).await;
    let reading_errors: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM yolink_readings_bookkeeping WHERE last_error IS NOT NULL",
    )
    .fetch_one(&pool)
    .await
    .unwrap_or(0);
    let reading_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM yolink_readings")
        .fetch_one(&pool)
        .await
        .context("count yolink_readings")?;

    Ok(Parsed::Fresh(Box::new(ParsedYolink {
        head,
        scan_elapsed,
        devices,
        series,
        commits,
        scope_config,
        reading_errors,
        reading_count,
    })))
}

async fn load_devices(pool: &SqlitePool) -> Result<Vec<DeviceRow>> {
    let rows = sqlx::query(
        "SELECT id, kind, start_ms, last_ts_ms, family_device_id \
           FROM yolink_devices ORDER BY id",
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
           FROM yolink_readings ORDER BY device_name, metric, ts_ms",
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
            Some(s) if s.device == device && s.metric == metric => {
                s.ts_ms.push(ts_ms);
                s.values.push(value);
            }
            _ => out.push(Series {
                device,
                metric,
                ts_ms: vec![ts_ms],
                values: vec![value],
            }),
        }
    }
    Ok(out)
}

/// `dolt_log()`, newest first. Best-effort: a store opened through a
/// libsqlite3 without doltlite's SQL surface has no commit log, and a
/// missing provenance section is not worth failing a render over.
async fn load_commits(pool: &SqlitePool) -> Vec<CommitRow> {
    let Ok(rows) = sqlx::query(
        "SELECT commit_hash, date, message FROM dolt_log() ORDER BY date DESC LIMIT 50",
    )
    .fetch_all(pool)
    .await
    else {
        return Vec::new();
    };
    rows.into_iter()
        .map(|r| CommitRow {
            hash: r.get::<String, _>("commit_hash"),
            date: r.get::<String, _>("date"),
            message: r.get::<String, _>("message"),
        })
        .collect()
}

/// The download step's recorded scope. Best-effort for the same reason
/// as [`load_commits`]: an older store may predate the table.
async fn load_scope_config(pool: &SqlitePool) -> Vec<ScopeConfigRow> {
    let Ok(rows) =
        sqlx::query("SELECT scope, config, updated_at FROM sync_scope_config ORDER BY scope")
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

impl ParsedYolink {
    /// Series grouped by device name, preserving the query's ordering.
    pub fn series_by_device(&self) -> BTreeMap<&str, Vec<&Series>> {
        let mut out: BTreeMap<&str, Vec<&Series>> = BTreeMap::new();
        for s in &self.series {
            out.entry(s.device.as_str()).or_default().push(s);
        }
        out
    }

    /// Newest reading timestamp anywhere in the store, if any.
    pub fn latest_ts_ms(&self) -> Option<i64> {
        self.series
            .iter()
            .filter_map(|s| s.ts_ms.last())
            .max()
            .copied()
    }

    /// Oldest reading timestamp anywhere in the store, if any.
    pub fn earliest_ts_ms(&self) -> Option<i64> {
        self.series
            .iter()
            .filter_map(|s| s.ts_ms.first())
            .min()
            .copied()
    }
}

impl Series {
    pub fn len(&self) -> usize {
        self.ts_ms.len()
    }
    pub fn is_empty(&self) -> bool {
        self.ts_ms.is_empty()
    }
}
