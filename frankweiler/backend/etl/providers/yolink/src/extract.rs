//! Yolink download → doltlite. For-loop over devices, inner loop
//! over forward-walking time windows: curl, parse, upsert, commit.
//!
//! Per-window `dolt_commit` is the load-bearing design choice — it
//! gives `dolt log` one entry per fetched window and lets `dolt
//! diff <prev>..<this>` surface server-side edits to historical
//! data. UPSERT only writes the row when the value actually
//! changed, so commits stay clean.
//!
//! Strict CSV header check: a `℃` column with a `℉` row value is
//! rejected, not coerced. The point is to notice unit flips
//! instead of corrupting history.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, NaiveDate, TimeZone, Utc};
use serde::Serialize;
use sqlx::sqlite::SqlitePool;
use tokio::process::Command;
use tracing::{info, warn};

use frankweiler_core::config::{YolinkDevice, YolinkSync};
use frankweiler_etl::control::ExtractControl;
use frankweiler_etl::doltlite_raw as dr;
use frankweiler_etl::progress::Progress;

pub use frankweiler_etl::doltlite_raw::db_path_for;

const DEFAULT_OVERLAP_MINUTES: i64 = 5;
const DEFAULT_WINDOW_DAYS: i64 = 1;

// ── parser ──────────────────────────────────────────────────────────

/// One parsed sample. Serializable so insta snapshot tests can
/// pretty-print it.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Reading {
    pub ts_ms: i64,
    pub metric: &'static str,
    pub value: f64,
}

/// Expected columns per device kind: `(header, metric, suffix)`.
/// `suffix=""` means values are bare numeric; otherwise the per-row
/// value must end with the suffix (e.g. `-18.4℃`) or we reject it.
fn columns_for(kind: &str) -> Result<&'static [(&'static str, &'static str, &'static str)]> {
    Ok(match kind {
        "temperature_humidity" => &[
            ("Temperature(℃)", "temperature_c", "℃"),
            ("Humidity(%RH)", "humidity_pct", ""),
        ],
        "watermeter" => &[
            ("Water Meter(GAL)", "water_meter_gal", ""),
            ("Water Consumption(GAL)", "water_consumption_gal", ""),
        ],
        other => bail!("unknown yolink device kind {other:?}"),
    })
}

pub fn parse(body: &str, kind: &str) -> Result<Vec<Reading>> {
    let cols = columns_for(kind)?;
    let mut rdr = csv::ReaderBuilder::new()
        .has_headers(true)
        .flexible(true)
        .from_reader(body.as_bytes());
    let headers = rdr.headers().context("read CSV header")?.clone();
    let find = |want: &str| {
        headers
            .iter()
            .position(|h| h == want)
            .ok_or_else(|| anyhow!("missing CSV column {want:?} (got {:?})", headers))
    };
    let time_idx = find("Time")?;
    let val_idxs: Vec<usize> = cols
        .iter()
        .map(|(h, _, _)| find(h))
        .collect::<Result<_>>()?;

    let mut out = Vec::new();
    for (i, rec) in rdr.records().enumerate() {
        let rec = rec.with_context(|| format!("row {}", i + 2))?;
        let Some(ts) = rec.get(time_idx) else {
            continue;
        };
        let ts_ms = DateTime::parse_from_str(ts, "%Y/%m/%d %H:%M:%S%z")
            .with_context(|| format!("row {}: bad ts {ts:?}", i + 2))?
            .timestamp_millis();
        for ((_, metric, suffix), &idx) in cols.iter().zip(&val_idxs) {
            let Some(raw) = rec.get(idx).filter(|s| !s.is_empty()) else {
                continue;
            };
            // `strip_suffix("")` succeeds and returns `raw` unchanged,
            // so bare-numeric columns (suffix == "") flow through the
            // same path without a special-case branch.
            let numeric = raw.strip_suffix(suffix).ok_or_else(|| {
                anyhow!(
                    "row {} {metric}: value {raw:?} missing suffix {suffix:?}",
                    i + 2
                )
            })?;
            let value = numeric
                .parse::<f64>()
                .with_context(|| format!("row {} {metric}: parse {numeric:?}", i + 2))?;
            out.push(Reading {
                ts_ms,
                metric,
                value,
            });
        }
    }
    Ok(out)
}

// ── doltlite store ──────────────────────────────────────────────────

const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS yolink_devices (
        id TEXT PRIMARY KEY,
        url_template TEXT NOT NULL,
        kind TEXT NOT NULL,
        start_ms INTEGER NOT NULL,
        last_ts_ms INTEGER NULL
    )",
    "CREATE TABLE IF NOT EXISTS yolink_readings (
        id TEXT PRIMARY KEY,
        device_name TEXT NOT NULL,
        ts_ms INTEGER NOT NULL,
        metric TEXT NOT NULL,
        value REAL NOT NULL
    )",
    "CREATE INDEX IF NOT EXISTS yolink_readings_by_device_ts
        ON yolink_readings(device_name, ts_ms)",
];

/// Thin wrapper around the doltlite pool — open + reset is all the
/// sync runner consumes externally. Everything else stays inline in
/// [`fetch`].
#[derive(Clone, Debug)]
pub struct RawDb {
    pool: SqlitePool,
}

impl RawDb {
    pub async fn open(db_path: &Path) -> Result<Self> {
        let pool = dr::open(db_path, DDL).await?;
        Ok(Self { pool })
    }
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
    pub async fn reset(&self) -> Result<()> {
        for sql in [
            "DELETE FROM yolink_devices",
            "DELETE FROM yolink_readings",
            "DELETE FROM blobs",
            "DELETE FROM blobs_bookkeeping",
        ] {
            sqlx::query(sql).execute(&self.pool).await?;
        }
        Ok(())
    }
}

fn reading_pk(device: &str, ts_ms: i64, metric: &str) -> String {
    format!("{device}#{ts_ms}#{metric}")
}

/// UPSERT one window's worth of readings. Returns
/// `(touched, unchanged)` — touched is insert + value-change (what
/// `dolt diff` will surface), unchanged is no-op PK conflicts.
/// The `WHERE value <> excluded.value` guard means `rows_affected`
/// counts only writes the DB actually performed; unchanged is
/// `total - touched`.
async fn upsert_readings(
    pool: &SqlitePool,
    device: &str,
    readings: &[Reading],
) -> Result<(usize, usize)> {
    let mut tx = pool.begin().await?;
    let mut touched: usize = 0;
    for r in readings {
        let res = sqlx::query(
            "INSERT INTO yolink_readings (id, device_name, ts_ms, metric, value)
             VALUES (?, ?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET value = excluded.value
                WHERE yolink_readings.value <> excluded.value",
        )
        .bind(reading_pk(device, r.ts_ms, r.metric))
        .bind(device)
        .bind(r.ts_ms)
        .bind(r.metric)
        .bind(r.value)
        .execute(&mut *tx)
        .await?;
        touched += res.rows_affected() as usize;
    }
    tx.commit().await?;
    Ok((touched, readings.len() - touched))
}

// ── orchestrator ────────────────────────────────────────────────────

pub struct FetchOptions {
    pub db_path: PathBuf,
    pub db: Option<RawDb>,
    pub sync: YolinkSync,
    pub progress: Progress,
    pub control: ExtractControl,
}

#[derive(Debug, Default, Clone)]
pub struct FetchSummary {
    pub devices: usize,
    pub windows: usize,
    pub readings_touched: usize,
    pub readings_unchanged: usize,
    pub commits: usize,
    pub errors: usize,
    pub requests: usize,
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db = match opts.db {
        Some(d) => d,
        None => RawDb::open(&db_path_for(&opts.db_path)).await?,
    };
    if opts.control.reset_and_redownload {
        db.reset().await?;
    }
    let overlap_ms = opts.sync.overlap_minutes.unwrap_or(DEFAULT_OVERLAP_MINUTES) * 60_000;
    let window_ms = opts.sync.window_days.unwrap_or(DEFAULT_WINDOW_DAYS) * 86_400_000;
    let mut s = FetchSummary {
        devices: opts.sync.devices.len(),
        ..Default::default()
    };
    opts.progress
        .set_length(Some(opts.sync.devices.len() as u64));
    let now_ms = Utc::now().timestamp_millis();
    for dev in &opts.sync.devices {
        opts.progress.set_message(&format!("yolink: {}", dev.name));
        if let Err(e) = fetch_device(&db, dev, overlap_ms, window_ms, now_ms, &mut s).await {
            s.errors += 1;
            warn!(event = "yolink_device_failed", device = %dev.name, error = %e);
        }
        opts.progress.inc(1);
    }
    Ok(s)
}

async fn fetch_device(
    db: &RawDb,
    dev: &YolinkDevice,
    overlap_ms: i64,
    window_ms: i64,
    now_ms: i64,
    s: &mut FetchSummary,
) -> Result<()> {
    let start_ms = NaiveDate::parse_from_str(&dev.start, "%Y-%m-%d")
        .with_context(|| format!("device {:?} start", dev.name))?
        .and_hms_opt(0, 0, 0)
        .map(|dt| Utc.from_utc_datetime(&dt).timestamp_millis())
        .unwrap();
    sqlx::query(
        "INSERT INTO yolink_devices (id, url_template, kind, start_ms) VALUES (?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET url_template=excluded.url_template, kind=excluded.kind, start_ms=excluded.start_ms",
    )
    .bind(&dev.name)
    .bind(&dev.url)
    .bind(&dev.kind)
    .bind(start_ms)
    .execute(db.pool())
    .await?;

    let watermark: Option<i64> =
        sqlx::query_scalar("SELECT last_ts_ms FROM yolink_devices WHERE id = ?")
            .bind(&dev.name)
            .fetch_one(db.pool())
            .await?;
    let mut cursor = watermark
        .map(|w| (w - overlap_ms).max(start_ms))
        .unwrap_or(start_ms);

    info!(event = "yolink_begin", device = %dev.name, cursor, now_ms);

    while cursor < now_ms {
        let end = cursor.saturating_add(window_ms).min(now_ms);
        let url = dev
            .url
            .replace("{start}", &cursor.to_string())
            .replace("{end}", &end.to_string());
        let body = curl(&url)
            .await
            .with_context(|| format!("{} {cursor}..{end}", dev.name))?;
        s.requests += 1;
        s.windows += 1;
        let rows =
            parse(&body, &dev.kind).with_context(|| format!("{} {cursor}..{end}", dev.name))?;
        let (touched, unchanged) = upsert_readings(db.pool(), &dev.name, &rows).await?;
        s.readings_touched += touched;
        s.readings_unchanged += unchanged;
        // Always attempt a per-window commit. Doltlite's
        // `dolt_commit('-Am', ...)` returns NULL (→ `Ok(None)`) when
        // nothing's dirty, so no-op windows leave `dolt log` clean
        // without us having to guard the call.
        let msg = format!(
            "yolink {} [{cursor}..{end}]: +{touched} ={unchanged}",
            dev.name
        );
        match dr::commit_run(db.pool(), &msg).await {
            Ok(Some(_)) => s.commits += 1,
            Ok(None) => {}
            Err(e) => warn!(event = "yolink_commit_failed", device = %dev.name, error = %e),
        }
        info!(event = "yolink_window", device = %dev.name, cursor, end, touched, unchanged);
        cursor = (end - overlap_ms).max(cursor + 1);
    }

    sqlx::query(
        "UPDATE yolink_devices SET last_ts_ms =
            (SELECT MAX(ts_ms) FROM yolink_readings WHERE device_name = ?)
         WHERE id = ?",
    )
    .bind(&dev.name)
    .bind(&dev.name)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// `curl -sSfL <url>` → stdout. `-f` makes 4xx/5xx exit non-zero so
/// we don't feed a "Forbidden" HTML body to the CSV parser.
async fn curl(url: &str) -> Result<String> {
    let out = Command::new("curl")
        .arg("-sSfL")
        .arg(url)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn curl")?
        .wait_with_output()
        .await?;
    out.status.success().then_some(()).ok_or_else(|| {
        anyhow!(
            "curl exit {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )
    })?;
    String::from_utf8(out.stdout).context("response not UTF-8")
}

// ── tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const TH: &str = "Device Id,Time,Temperature(℃),Humidity(%RH)\n\
        d88b,2026/04/05 17:02:04-0700,-18.4℃,70.0\n\
        d88b,2026/04/05 17:05:34-0700,-18.0℃,\n";
    const WM: &str = "Device Id,Time,Water Meter(GAL),Water Consumption(GAL)\n\
        d88b,2026/04/05 17:00:00-0700,529.084,0.000\n\
        d88b,2026/04/05 17:02:36-0700,529.374,0.291\n";

    #[test]
    fn parse_thsensor() {
        insta::assert_yaml_snapshot!(parse(TH, "temperature_humidity").unwrap());
    }

    #[test]
    fn parse_watermeter() {
        insta::assert_yaml_snapshot!(parse(WM, "watermeter").unwrap());
    }

    #[test]
    fn parse_rejects_unit_flips() {
        let bad_header =
            "Device Id,Time,Temperature(℉),Humidity(%RH)\nx,2026/04/05 17:02:04-0700,-1.1℉,70.0\n";
        let bad_row =
            "Device Id,Time,Temperature(℃),Humidity(%RH)\nx,2026/04/05 17:02:04-0700,-1.1℉,70.0\n";
        insta::assert_snapshot!(
            "bad_header",
            format!(
                "{:#}",
                parse(bad_header, "temperature_humidity").unwrap_err()
            )
        );
        insta::assert_snapshot!(
            "bad_row",
            format!("{:#}", parse(bad_row, "temperature_humidity").unwrap_err())
        );
    }

    #[tokio::test]
    async fn upsert_distinguishes_new_changed_unchanged() {
        let dir = tempfile::tempdir().unwrap();
        let db = RawDb::open(&dir.path().join("yl.doltlite_db"))
            .await
            .unwrap();
        let pool = db.pool();
        let r = |ts, v| Reading {
            ts_ms: ts,
            metric: "water_meter_gal",
            value: v,
        };
        assert_eq!(
            upsert_readings(pool, "v", &[r(100, 1.0), r(200, 2.0)])
                .await
                .unwrap(),
            (2, 0)
        );
        assert_eq!(
            upsert_readings(pool, "v", &[r(100, 1.0), r(200, 2.0)])
                .await
                .unwrap(),
            (0, 2)
        );
        assert_eq!(
            upsert_readings(pool, "v", &[r(100, 1.5), r(200, 2.0)])
                .await
                .unwrap(),
            (1, 1)
        );
    }
}
