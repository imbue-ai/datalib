//! Yolink download → doltlite. For-loop over devices, inner loop
//! over forward-walking time windows: curl, parse, upsert. No
//! per-window `dolt_commit`: the sync orchestrator wraps the whole
//! extract in one commit when [`fetch`] returns, which is the right
//! grain (a sync run is a single "snapshot of upstream"). UPSERT
//! still only writes when a value actually changed, so the trailing
//! commit's diff is exactly the readings that moved this run.
//!
//! Strict CSV header check: a `℃` column with a `℉` row value is
//! rejected, not coerced. The point is to notice unit flips
//! instead of corrupting history.

pub mod schema_raw;

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{NaiveDate, TimeZone, Utc};
use md5::{Digest, Md5};
use serde::Serialize;
use sqlx::sqlite::SqlitePool;
use tokio::process::Command;
use tracing::{info, warn};

use frankweiler_core::config::{YolinkDevice, YolinkSync};
use frankweiler_etl::control::ExtractControl;
use frankweiler_etl::doltlite_raw as dr;
use frankweiler_etl::progress::Progress;

use schema_raw::{full_ddl, reading_id_recipe, DATA_TABLES};

pub use frankweiler_etl::doltlite_raw::db_path_for;

const DEFAULT_OVERLAP_MINUTES: i64 = 5;
/// Stride between successive window-starts, in days. Each fetched
/// window is `stride + overlap` wide so the cursor lands on
/// `start + n * stride` every iteration — meaning all devices that
/// share a `start:` date hit Yolink with the *same* (start_ms, end_ms)
/// pair each run, which cuts request count if the user later adds
/// per-device download caching. The default of 7 keeps the
/// `dolt_commit`-per-window history weekly-grained.
const DEFAULT_WINDOW_DAYS: i64 = 7;

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
        let ts_ms = frankweiler_time::parse_custom_strftime(ts, "%Y/%m/%d %H:%M:%S%z")
            .with_context(|| format!("row {}: bad ts {ts:?}", i + 2))?
            .inner()
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

/// Thin wrapper around the doltlite pool — open + reset is all the
/// sync runner consumes externally. Everything else stays inline in
/// [`fetch`].
#[derive(Clone, Debug)]
pub struct RawDb {
    pool: SqlitePool,
}

impl RawDb {
    pub async fn open(db_path: &Path) -> Result<Self> {
        let owned = full_ddl();
        let slices: Vec<&str> = owned.iter().map(String::as_str).collect();
        let pool = dr::open(db_path, &slices).await?;
        Ok(Self { pool })
    }
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }
    pub async fn reset(&self) -> Result<()> {
        for table in DATA_TABLES {
            sqlx::query(&format!("DELETE FROM {table}"))
                .execute(&self.pool)
                .await?;
        }
        Ok(())
    }
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
        .bind(reading_id_recipe(device, r.ts_ms, r.metric))
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
    let stride_ms = opts.sync.window_days.unwrap_or(DEFAULT_WINDOW_DAYS) * 86_400_000;
    let window_ms = stride_ms.saturating_add(overlap_ms);
    let mut s = FetchSummary {
        devices: opts.sync.devices.len(),
        ..Default::default()
    };
    opts.progress
        .set_length(Some(opts.sync.devices.len() as u64));
    let now_ms = Utc::now().timestamp_millis();
    for dev in &opts.sync.devices {
        opts.progress.set_message(&format!("yolink: {}", dev.name));
        if let Err(e) =
            fetch_device(&db, dev, overlap_ms, stride_ms, window_ms, now_ms, &mut s).await
        {
            s.errors += 1;
            warn!(event = "yolink_device_failed", device = %dev.name, error = %format!("{e:#}"));
        }
        opts.progress.inc(1);
    }
    Ok(s)
}

async fn fetch_device(
    db: &RawDb,
    dev: &YolinkDevice,
    overlap_ms: i64,
    stride_ms: i64,
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
        "INSERT INTO yolink_devices (id, family_device_id, kind, start_ms) VALUES (?, ?, ?, ?)
         ON CONFLICT(id) DO UPDATE SET family_device_id=excluded.family_device_id, kind=excluded.kind, start_ms=excluded.start_ms",
    )
    .bind(&dev.name)
    .bind(&dev.family_device_id)
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

    // Tolerate per-window failures (a single 4xx or transient curl error
    // shouldn't take out an entire device's backfill — common when the
    // configured `start` predates when the device was deployed). Advance
    // the cursor on failure and keep marching. Hard-fail only after
    // CONSECUTIVE_FAILURE_BUDGET in a row, so a stuck credential or
    // bad URL still surfaces instead of silently looping for years.
    const CONSECUTIVE_FAILURE_BUDGET: u32 = 30;
    let mut consecutive_failures: u32 = 0;

    while cursor < now_ms {
        let end = cursor.saturating_add(window_ms).min(now_ms);
        let url = build_signed_url(dev, cursor, end)?;
        let window_result = async {
            let body = curl(&url).await.context("curl")?;
            s.requests += 1;
            s.windows += 1;
            let rows = parse(&body, &dev.kind).context("parse")?;
            let (touched, unchanged) = upsert_readings(db.pool(), &dev.name, &rows).await?;
            Ok::<_, anyhow::Error>((touched, unchanged))
        }
        .await;
        let (touched, unchanged) = match window_result {
            Ok(v) => {
                consecutive_failures = 0;
                v
            }
            Err(e) => {
                consecutive_failures += 1;
                warn!(
                    event = "yolink_window_failed",
                    device = %dev.name,
                    cursor, end,
                    consecutive_failures,
                    error = %format!("{e:#}"),
                );
                if consecutive_failures >= CONSECUTIVE_FAILURE_BUDGET {
                    return Err(e.context(format!(
                        "{} aborted after {consecutive_failures} consecutive window failures (last window {cursor}..{end})",
                        dev.name
                    )));
                }
                cursor = cursor.saturating_add(stride_ms).max(cursor + 1);
                continue;
            }
        };
        s.readings_touched += touched;
        s.readings_unchanged += unchanged;
        info!(event = "yolink_window", device = %dev.name, cursor, end, touched, unchanged);
        cursor = cursor.saturating_add(stride_ms).max(cursor + 1);
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

/// Compose and sign the per-window CSV download URL. The signature
/// is `md5(family_device_id + start_ms + end_ms + device_udid)` —
/// reverse-engineered from the Safehous/YoLink Android Flutter
/// snapshot (see `ParamUtils::hashMD5` + `_THSensorNewChartScreenState`).
/// Yolink does not expose this scheme via its public API; UAC tokens
/// can't access historical data.
///
/// REDACT: the `family_device_id` + `device_udid` pair from each
/// `YolinkDevice` is a per-device read secret. Anything that publishes
/// generated URLs effectively publishes that secret.
fn build_signed_url(dev: &YolinkDevice, start_ms: i64, end_ms: i64) -> Result<String> {
    let mut hasher = Md5::new();
    hasher.update(dev.family_device_id.as_bytes());
    hasher.update(start_ms.to_string().as_bytes());
    hasher.update(end_ms.to_string().as_bytes());
    hasher.update(dev.device_udid.as_bytes());
    let sig = format!("{:x}", hasher.finalize());

    // Per-kind query params. `extParams` is a base64-url JSON blob the
    // app appends to control CSV content (humidity inclusion for the
    // THSensor; meter unit + step factor for the watermeter). It is
    // NOT part of the signature input — server only signs (family,
    // start, end, udid) — so we can hardcode reasonable defaults that
    // match the captured live URLs.
    let (ext_params, temp_unit) = match dev.kind.as_str() {
        "temperature_humidity" => (
            // {"ignoreHumidity":false}
            "eyJpZ25vcmVIdW1pZGl0eSI6ZmFsc2V9",
            Some("c"),
        ),
        "watermeter" => (
            // {"meterUnit":3,"meterScreenUnit":0,"stepFactor":10}
            "eyJtZXRlclVuaXQiOjMsIm1ldGVyU2NyZWVuVW5pdCI6MCwic3RlcEZhY3RvciI6MTB9",
            None,
        ),
        other => bail!("unsupported yolink device kind {other:?}"),
    };
    let mut url = format!(
        "https://us.yosmart.com/download/{fam}/{sig}?start={start_ms}&end={end_ms}",
        fam = dev.family_device_id,
    );
    if let Some(unit) = temp_unit {
        url.push_str("&tempUnit=");
        url.push_str(unit);
    }
    url.push_str("&tz=UTC&original=true&extParams=");
    url.push_str(ext_params);
    Ok(url)
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
