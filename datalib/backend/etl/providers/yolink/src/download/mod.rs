//! Yolink download → doltlite. For-loop over devices, inner loop
//! over forward-walking time windows: curl, parse, bulk-upsert. No
//! per-window `dolt_commit`: the sync orchestrator wraps the whole
//! download in one commit when [`fetch`] returns, which is the right
//! grain (a sync run is a single "snapshot of upstream"). `dolt
//! diff` against that trailing commit shows exactly which readings
//! moved this run — same source-of-truth pattern every other
//! provider uses.
//!
//! Demo of how little this layer needs once the schema is in place
//! — the row types and their `BulkUpsertable` impls live in
//! [`schema_raw`]; this file is just curl / parse / loop, plus the
//! per-device cursor-advance after each window. See
//! [`docs/dev/data_architecture_ingestion.md`] §"Schema first" for the
//! principle this provider was kept simple to demonstrate.
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

use datalib_etl::bulk::bulk_upsert_in_tx;
use datalib_etl::control::DownloadControl;
use datalib_etl::doltlite_raw as dr;
use datalib_etl::progress::Progress;
use datalib_etl_yolink_config::{YolinkDevice, YolinkSync};

use schema_raw::{full_ddl, YolinkDeviceRow, YolinkReadingRow, DATA_TABLES};

pub use datalib_etl::doltlite_raw::db_path_for;

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
///
/// `payload` is the JSON-encoded `{header: value}` map of the source
/// CSV row this sample was derived from — the raw wire representation
/// YoLink served. Two `Reading`s coming out of the same CSV row (e.g.
/// the temperature + humidity pair from a `Temperature(℃) Humidity
/// (%RH)` row) share an identical `payload` string; see the
/// [`schema_raw::YolinkReadingRow`] docstring for why we keep this
/// even though it costs a small amount of denormalization.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Reading {
    pub ts_ms: i64,
    pub metric: &'static str,
    pub value: f64,
    pub payload: String,
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
        let ts_ms = datalib_time::parse_custom_strftime(ts, "%Y/%m/%d %H:%M:%S%z")
            .with_context(|| format!("row {}: bad ts {ts:?}", i + 2))?
            .inner()
            .timestamp_millis();
        // Build the per-CSV-row payload once: `{header: value}` for
        // every column in the source record. Every Reading derived
        // from this CSV row carries the same payload string, so the
        // raw wire representation survives even after the typed
        // columns strip unit suffixes / parse numerics.
        let payload = {
            let mut m = serde_json::Map::with_capacity(headers.len());
            for (h, v) in headers.iter().zip(rec.iter()) {
                m.insert(h.to_string(), serde_json::Value::String(v.to_string()));
            }
            serde_json::Value::Object(m).to_string()
        };
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
                payload: payload.clone(),
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

/// UPSERT one window's worth of readings through the shared
/// [`bulk_upsert_in_tx`] helper. Same per-tx batching every other
/// provider uses; `dolt diff` against the trailing
/// orchestrator-level commit is the source of truth for what
/// actually changed.
async fn upsert_readings(pool: &SqlitePool, device: &str, readings: &[Reading]) -> Result<usize> {
    if readings.is_empty() {
        return Ok(0);
    }
    let rows: Vec<YolinkReadingRow> = readings
        .iter()
        .map(|r| YolinkReadingRow::new(device, r.ts_ms, r.metric, r.value, r.payload.clone()))
        .collect();
    let now = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    let mut tx = pool.begin().await?;
    bulk_upsert_in_tx(&mut tx, &rows, &now).await?;
    tx.commit().await?;
    Ok(readings.len())
}

// ── orchestrator ────────────────────────────────────────────────────

pub struct FetchOptions {
    pub db_path: PathBuf,
    pub db: Option<RawDb>,
    pub sync: YolinkSync,
    pub progress: Progress,
    pub control: DownloadControl,
}

#[derive(Debug, Default, Clone)]
pub struct FetchSummary {
    pub devices: usize,
    pub windows: usize,
    /// Total readings seen across every window this run. To know what
    /// actually CHANGED, check `dolt diff` against the prior commit —
    /// that's the universal source of truth.
    pub readings: usize,
    pub errors: usize,
    pub requests: usize,
}

/// Scope key for this provider's [`frankweiler_etl::scope_config`] blob.
const SCOPE_CONFIG_KEY: &str = "yolink:download";

/// Blob key. Named so writer and reader can't drift.
const K_DEVICE_STARTS: &str = "device_starts";

/// Per-device `start` dates, the only knob that decides which data lands
/// on disk. `overlap_minutes` / `window_days` shape *how* the walk
/// paginates and are re-applied every run, so they don't belong here.
fn scope_config_blob(opts: &FetchOptions) -> serde_json::Value {
    let starts: std::collections::BTreeMap<&str, &str> = opts
        .sync
        .devices
        .iter()
        .map(|d| (d.name.as_str(), d.start.as_str()))
        .collect();
    serde_json::json!({ K_DEVICE_STARTS: starts })
}

/// The `start` this device had on the last run that satisfied the
/// config, if any. Keyed by device name — the same key that keys the
/// row's history, so renaming a device reads as a new device (which it
/// effectively is; see `YolinkDevice::name`).
fn prior_start_for(prior: Option<&serde_json::Value>, name: &str) -> Option<String> {
    prior?
        .get(K_DEVICE_STARTS)?
        .get(name)?
        .as_str()
        .map(str::to_string)
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    // Built before `opts.db` is moved out below.
    let scope_cfg = scope_config_blob(&opts);
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
    // Diff the per-device `start` dates against the ones that produced
    // the stored watermarks. `None` (fresh store, or one written before
    // `sync_scope_config` existed) plans no backfill.
    let prior_scope_cfg =
        frankweiler_etl::scope_config::load_or_none(db.pool(), SCOPE_CONFIG_KEY).await;
    for dev in &opts.sync.devices {
        opts.progress.set_message(&format!("yolink: {}", dev.name));
        let prior_start = prior_start_for(prior_scope_cfg.as_ref(), &dev.name);
        if let Err(e) = fetch_device(
            &db,
            dev,
            prior_start.as_deref(),
            overlap_ms,
            stride_ms,
            window_ms,
            now_ms,
            &mut s,
        )
        .await
        {
            s.errors += 1;
            warn!(event = "yolink_device_failed", device = %dev.name, error = %format!("{e:#}"));
        }
        opts.progress.inc(1);
    }
    // Record the config only when every device succeeded: a device that
    // errored hasn't covered its widened `start`, and the blob is one
    // row for all of them.
    frankweiler_etl::scope_config::store_if_satisfied(
        db.pool(),
        SCOPE_CONFIG_KEY,
        &scope_cfg,
        s.errors == 0,
    )
    .await;
    Ok(s)
}

/// What the resume decision did, for the caller to log.
#[derive(Debug, PartialEq, Eq)]
enum CursorNote {
    /// Resumed from the watermark (clamped forward to `start`, which is
    /// a floor). The ordinary case.
    Normal,
    /// `start` moved earlier than the run that produced the watermark,
    /// so the range below the old start was never walked. Reset to the
    /// new start; windows are UPSERT-deduped, so re-walking the overlap
    /// costs requests, not correctness.
    Backfill,
    /// `start` moved later, past the watermark. The clamp jumps the
    /// cursor forward and `[watermark, start]` is never fetched. That is
    /// what `start` literally asks for, so it's preserved — but said out
    /// loud rather than done silently.
    SkipsAhead,
}

/// Where this run should begin walking for one device.
///
/// Pure so the config-change branches are testable without a transport;
/// `fetch_device` shells out to curl.
fn resume_cursor(
    watermark: Option<i64>,
    start_ms: i64,
    overlap_ms: i64,
    start_widened: bool,
    start_narrowed: bool,
) -> (i64, CursorNote) {
    match watermark {
        _ if start_widened => (start_ms, CursorNote::Backfill),
        None => (start_ms, CursorNote::Normal),
        Some(w) => {
            let clamped = (w - overlap_ms).max(start_ms);
            let note = if start_narrowed && clamped > w {
                CursorNote::SkipsAhead
            } else {
                CursorNote::Normal
            };
            (clamped, note)
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn fetch_device(
    db: &RawDb,
    dev: &YolinkDevice,
    prior_start: Option<&str>,
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
    let device_row = YolinkDeviceRow {
        id: dev.name.clone(),
        family_device_id: dev.family_device_id.clone(),
        kind: dev.kind.clone(),
        start_ms,
    };
    let now = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    let mut tx = db.pool().begin().await?;
    bulk_upsert_in_tx(&mut tx, &[device_row], &now).await?;
    tx.commit().await?;

    let watermark: Option<i64> =
        sqlx::query_scalar("SELECT last_ts_ms FROM yolink_devices WHERE id = ?")
            .bind(&dev.name)
            .fetch_one(db.pool())
            .await?;

    // Only a *recorded* move counts. Comparing `start` to the watermark
    // alone would fire on every run of any store whose configured start
    // simply sits ahead of its data. `YYYY-MM-DD` sorts lexicographically
    // as it does chronologically (validated at config load).
    let start_widened = prior_start.is_some_and(|p| dev.start.as_str() < p);
    let start_narrowed = prior_start.is_some_and(|p| dev.start.as_str() > p);

    let (cursor_start, note) = resume_cursor(
        watermark,
        start_ms,
        overlap_ms,
        start_widened,
        start_narrowed,
    );
    match note {
        CursorNote::Backfill => info!(
            event = "yolink_start_widened",
            device = %dev.name,
            from = prior_start.unwrap_or_default(),
            to = %dev.start,
            "re-walking from the new start",
        ),
        CursorNote::SkipsAhead => warn!(
            event = "yolink_start_skips_ahead",
            device = %dev.name,
            from = prior_start.unwrap_or_default(),
            to = %dev.start,
            "start moved past the stored watermark; the range between \
             them will not be fetched",
        ),
        CursorNote::Normal => {}
    }
    let mut cursor = cursor_start;

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
            let upserted = upsert_readings(db.pool(), &dev.name, &rows).await?;
            Ok::<_, anyhow::Error>(upserted)
        }
        .await;
        let upserted = match window_result {
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
        s.readings += upserted;
        info!(event = "yolink_window", device = %dev.name, cursor, end, upserted);
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
    async fn upsert_readings_lands_rows() {
        let dir = tempfile::tempdir().unwrap();
        let db = RawDb::open(&dir.path().join("yl.doltlite_db"))
            .await
            .unwrap();
        let pool = db.pool();
        let r = |ts, v| Reading {
            ts_ms: ts,
            metric: "water_meter_gal",
            value: v,
            payload: "{}".to_string(),
        };
        // Two readings land.
        assert_eq!(
            upsert_readings(pool, "v", &[r(100, 1.0), r(200, 2.0)])
                .await
                .unwrap(),
            2
        );
        // Re-upsert is idempotent on row count (dolt diff is the
        // authority on "did anything actually change?").
        assert_eq!(upsert_readings(pool, "v", &[r(100, 1.5)]).await.unwrap(), 1);
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM yolink_readings")
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(n, 2, "second upsert updates, doesn't duplicate");
    }
}

#[cfg(test)]
mod scope_config_tests {
    use super::*;
    use serde_json::json;

    fn dev(name: &str, start: &str) -> YolinkDevice {
        YolinkDevice {
            name: name.into(),
            kind: "watermeter".into(),
            start: start.into(),
            family_device_id: "0123456789abcdef0123456789abcdef".into(),
            device_udid: "fedcba9876543210fedcba9876543210".into(),
        }
    }

    fn opts_with(devices: Vec<YolinkDevice>) -> FetchOptions {
        FetchOptions {
            db_path: std::path::PathBuf::new(),
            db: None,
            sync: YolinkSync {
                overlap_minutes: None,
                window_days: None,
                devices,
            },
            progress: Progress::noop(),
            control: DownloadControl::default(),
        }
    }

    #[test]
    fn blob_records_starts_keyed_by_device_name() {
        let blob = scope_config_blob(&opts_with(vec![dev("freezer", "2024-01-01")]));
        assert_eq!(blob, json!({"device_starts": {"freezer": "2024-01-01"}}));
    }

    #[test]
    fn blob_omits_pagination_knobs() {
        // `overlap_minutes` / `window_days` are re-applied every run, so
        // recording them would only provoke pointless re-walks.
        let mut o = opts_with(vec![dev("freezer", "2024-01-01")]);
        o.sync.overlap_minutes = Some(99);
        o.sync.window_days = Some(3);
        let obj = scope_config_blob(&o);
        let obj = obj.as_object().unwrap();
        assert_eq!(obj.len(), 1);
        assert!(obj.contains_key("device_starts"));
    }

    #[test]
    fn prior_start_reads_the_matching_device() {
        let blob = json!({"device_starts": {"freezer": "2024-01-01", "tank": "2023-06-01"}});
        assert_eq!(
            prior_start_for(Some(&blob), "freezer").as_deref(),
            Some("2024-01-01")
        );
        assert_eq!(
            prior_start_for(Some(&blob), "tank").as_deref(),
            Some("2023-06-01")
        );
        // A device not in the record is new: no prior, so no backfill.
        assert_eq!(prior_start_for(Some(&blob), "unknown"), None);
    }

    #[test]
    fn absent_prior_reads_as_no_information() {
        assert_eq!(prior_start_for(None, "freezer"), None);
        assert_eq!(prior_start_for(Some(&json!({})), "freezer"), None);
    }

    // ── resume_cursor ────────────────────────────────────────────────

    const HOUR: i64 = 3_600_000;

    #[test]
    fn cold_start_begins_at_start() {
        assert_eq!(
            resume_cursor(None, 1_000, 60_000, false, false),
            (1_000, CursorNote::Normal)
        );
    }

    #[test]
    fn watermark_resumes_with_overlap() {
        let (c, note) = resume_cursor(Some(100 * HOUR), HOUR, HOUR, false, false);
        assert_eq!(c, 99 * HOUR, "one overlap back from the watermark");
        assert_eq!(note, CursorNote::Normal);
    }

    #[test]
    fn start_is_a_floor_on_the_overlap() {
        // Overlap would reach below the configured start; clamp to it,
        // and that is the ordinary case, not a config change.
        let (c, note) = resume_cursor(Some(10 * HOUR), 9 * HOUR, 5 * HOUR, false, false);
        assert_eq!(c, 9 * HOUR);
        assert_eq!(note, CursorNote::Normal);
    }

    #[test]
    fn widened_start_resets_the_cursor() {
        // The whole point: a watermark far ahead does not suppress the
        // backfill when `start` moved earlier.
        let (c, note) = resume_cursor(Some(100 * HOUR), 2 * HOUR, HOUR, true, false);
        assert_eq!(c, 2 * HOUR);
        assert_eq!(note, CursorNote::Backfill);
    }

    #[test]
    fn narrowed_start_past_the_watermark_is_flagged() {
        let (c, note) = resume_cursor(Some(10 * HOUR), 50 * HOUR, HOUR, false, true);
        assert_eq!(c, 50 * HOUR, "start wins; the gap is what it asks for");
        assert_eq!(note, CursorNote::SkipsAhead);
    }

    #[test]
    fn stable_config_never_reports_skips_ahead() {
        // Without a recorded move, a start that simply sits ahead of the
        // watermark must not warn on every single run.
        let (_, note) = resume_cursor(Some(10 * HOUR), 50 * HOUR, HOUR, false, false);
        assert_eq!(note, CursorNote::Normal);
    }
}
