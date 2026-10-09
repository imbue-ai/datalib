//! YoLink download → doltlite. Each device's history is a range,
//! `[start, now]` cut down to what YoLink still serves, and `coverage`
//! records which stretches of it have been looked at, in the
//! transaction that stores what a stretch held, an empty stretch
//! included. What is left to fetch is the gaps, walked oldest first in
//! fixed-stride windows; a window that fails stays a gap for the next
//! run (docs/dev/data_architecture_ingestion.md, "What is left to fetch").

pub mod schema_raw;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{NaiveDate, TimeZone, Utc};
use md5::{Digest, Md5};
use serde::Serialize;
use sqlx::sqlite::SqlitePool;
use tracing::info;

use datalib_etl::bulk::bulk_upsert_in_tx;
use datalib_etl::control::DownloadControl;
use datalib_etl::download_problems::SilentEntry;
use datalib_etl::progress::{Progress, RunBar};
use datalib_etl::raw_store::Sealer;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl_web::coverage::{self, Span};
use datalib_etl_web::http::{latchkey_curl, HttpRequest, HttpService};
use datalib_etl_yolink_config::{YolinkDevice, YolinkSync};

use schema_raw::{device_scope, full_ddl, span_end, YolinkDeviceRow, YolinkReadingRow};

pub use datalib_etl::doltlite_raw::db_path_for;

/// Each window's request begins this far before the window, so the
/// readings at the previous window's end are asked for twice: the
/// newest few minutes of a run are the ones a sensor may not have
/// reported yet.
const DEFAULT_OVERLAP_MINUTES: i64 = 5;
/// The stride a gap is walked in, in days.
const DEFAULT_WINDOW_DAYS: i64 = 7;
/// A device with no reading this recent is reported as gone quiet. The
/// sensors report every few minutes, so a day is far past a gap.
const SILENT_AFTER_MS: i64 = 86_400_000;
/// How far back YoLink still serves history: about 66 days on the one
/// account measured (see INGEST.md). Nothing older is asked for, since
/// the answer would be empty whatever that stretch held.
const HISTORY_KEPT_MS: i64 = 66 * 86_400_000;
/// Failed windows in a row before a device is left for the next run: a
/// run this long is a stuck credential or a dead device, and each more
/// is a request for nothing.
const CONSECUTIVE_FAILURE_BUDGET: u32 = 30;

// ── parser ──────────────────────────────────────────────────────────

/// One parsed sample. Serializable so insta snapshot tests can
/// pretty-print it.
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
    // A device that sent nothing in the window gets an empty body, not
    // even the header row.
    if body.trim().is_empty() {
        return Ok(Vec::new());
    }
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

datalib_etl::raw_db!(
    /// Thin wrapper around the doltlite pool — open + reset is all the
    /// sync runner consumes externally. Everything else stays inline in
    /// [`fetch`].
    pub RawDb: EntityStore,
    full_ddl(),
    schema_raw::LADDER
);

/// One window's readings and the record that the window was looked at,
/// in one transaction: a window with no readings leaves the record and
/// nothing else.
async fn store_window(
    pool: &SqlitePool,
    device: &str,
    window: (i64, i64),
    readings: &[Reading],
) -> Result<usize> {
    let rows: Vec<YolinkReadingRow> = readings
        .iter()
        .map(|r| YolinkReadingRow::new(device, r.ts_ms, r.metric, r.value, r.payload.clone()))
        .collect();
    let now = datalib_time::IsoOffsetTimestamp::now_local();
    let mut tx = pool.begin().await?;
    if !rows.is_empty() {
        bulk_upsert_in_tx(&mut tx, &rows, &now).await?;
    }
    coverage::cover(
        &mut tx,
        &device_scope(device),
        Span::new(span_end(window.0), span_end(window.1)),
    )
    .await?;
    tx.commit().await?;
    Ok(readings.len())
}

// ── orchestrator ────────────────────────────────────────────────────

pub struct FetchOptions {
    /// The store this run writes into, opened and closed by the caller.
    /// A download never opens a store of its own: one writer per file
    /// (`datalib/backend/etl/README.md` § "One writer per file, by
    /// construction").
    pub db: RawDb,
    pub sync: YolinkSync,
    /// The run's pinned now, in epoch ms: where every walk ends and what
    /// silence is measured against.
    pub now_ms: i64,
    pub progress: Progress,
    pub control: DownloadControl,
    /// Told after each device's walk, so a long run publishes as it goes.
    pub sealer: Option<Sealer>,
}

#[derive(Debug, Default, Clone)]
pub struct FetchSummary {
    pub devices: usize,
    pub windows: usize,
    /// Total readings seen across every window this run. To know what
    /// actually CHANGED, check `dolt diff` against the prior commit —
    /// that's the universal source of truth.
    pub readings: usize,
    /// Devices that could not be planned or were abandoned.
    pub errors: usize,
    /// Windows that failed this run; each is still a gap.
    pub windows_failed: usize,
    pub requests: usize,
}

/// Where one window's CSV comes from: YoLink over the shared HTTP layer
/// in a run, canned answers in a test.
pub(crate) trait WindowSource {
    async fn csv(&self, dev: &YolinkDevice, start_ms: i64, end_ms: i64) -> Result<String>;
}

struct Http;

impl WindowSource for Http {
    async fn csv(&self, dev: &YolinkDevice, start_ms: i64, end_ms: i64) -> Result<String> {
        let resp = latchkey_curl(&window_request(dev, start_ms, end_ms)?).await?;
        if !(200..300).contains(&resp.status) {
            return Err(anyhow::Error::new(HttpStatus(resp.status)));
        }
        Ok(resp.body_str().into_owned())
    }
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    fetch_from(opts, &Http).await
}

pub(crate) async fn fetch_from<S: WindowSource>(
    opts: FetchOptions,
    src: &S,
) -> Result<FetchSummary> {
    let (pool, stop) = (opts.db.pool().clone(), opts.control.stop.clone());
    let sealer = opts.sealer.clone();
    run_problems::collecting_sealed(&pool, &stop, sealer.as_ref(), |found| {
        walk_devices(opts, src, found)
    })
    .await
}

/// `YYYY-MM-DD` as the epoch milliseconds of that day's start, UTC.
pub fn start_of_day_ms(date: &str) -> Option<i64> {
    let d = NaiveDate::parse_from_str(date, "%Y-%m-%d").ok()?;
    Some(
        Utc.from_utc_datetime(&d.and_hms_opt(0, 0, 0)?)
            .timestamp_millis(),
    )
}

/// The stretch of a device's history a run wants: from its configured
/// start, or from where YoLink's history begins if that is later, to
/// the run's now.
fn wanted(start_ms: i64, now_ms: i64) -> Span {
    Span::new(
        span_end(start_ms.max(now_ms - HISTORY_KEPT_MS)),
        span_end(now_ms),
    )
}

/// The windows that walk `[lo, hi]`, lowest first: `stride_ms` wide,
/// the last one cut short at `hi`.
pub fn windows_of(lo: i64, hi: i64, stride_ms: i64) -> Vec<(i64, i64)> {
    let stride = stride_ms.max(1);
    let mut out = Vec::new();
    let mut from = lo;
    while from < hi {
        let to = from.saturating_add(stride).min(hi);
        out.push((from, to));
        from = to;
    }
    out
}

/// The range YoLink is asked for to fill `window`: the window plus the
/// overlap before it, never before the device's configured start.
pub fn requested(window: (i64, i64), overlap_ms: i64, start_ms: i64) -> (i64, i64) {
    ((window.0 - overlap_ms).max(start_ms), window.1)
}

/// One device's share of the run.
struct Plan<'a> {
    dev: &'a YolinkDevice,
    start_ms: i64,
    /// The gaps in its coverage, as windows, lowest first.
    windows: Vec<(i64, i64)>,
}

async fn walk_devices<S: WindowSource>(
    opts: FetchOptions,
    src: &S,
    found: RunProblems,
) -> Result<FetchSummary> {
    let db = opts.db;
    let stop = &opts.control.stop;
    let overlap_ms = opts.sync.overlap_minutes.unwrap_or(DEFAULT_OVERLAP_MINUTES) * 60_000;
    let stride_ms = opts.sync.window_days.unwrap_or(DEFAULT_WINDOW_DAYS) * 86_400_000;
    if stride_ms <= 0 || overlap_ms < 0 {
        bail!("yolink: window_days must be at least 1 and overlap_minutes at least 0");
    }
    let mut s = FetchSummary {
        devices: opts.sync.devices.len(),
        ..Default::default()
    };
    let now_ms = opts.now_ms;

    // Every device's gaps first, so the bar counts requests (one per
    // window) rather than devices, which finish in uneven lumps.
    let mut plans = Vec::with_capacity(opts.sync.devices.len());
    for dev in &opts.sync.devices {
        match plan_device(&db, dev, now_ms, stride_ms).await? {
            Ok(plan) => plans.push(plan),
            Err(why) => {
                s.errors += 1;
                found.listing(&dev.name, why);
            }
        }
    }
    let bar = RunBar::new(
        &opts.progress,
        plans.iter().map(|p| p.windows.len() as u64).sum(),
    );

    for plan in &plans {
        bar.doing(&format!("yolink: {}", plan.dev.name));
        let walk = Walk {
            db: &db,
            src,
            dev: plan.dev,
            bar: &bar,
            stop,
            overlap_ms,
            start_ms: plan.start_ms,
        };
        let readings_before = s.readings;
        let walked = walk.run(&plan.windows, &mut s).await?;
        if let Some(sealer) = &opts.sealer {
            sealer.wrote((s.readings - readings_before) as u64).await;
        }
        match walked.end {
            WalkEnd::Done | WalkEnd::Stopped => {
                // The row's sample shows eighty characters: the cause first.
                if let Some(((why, window), _)) = walked.failed.split_first() {
                    found.listing(
                        &plan.dev.name,
                        format!(
                            "{why}; {} of {} windows could not be fetched and are asked \
                             for again next run; the first, {}..{}",
                            walked.failed.len(),
                            plan.windows.len(),
                            window.0,
                            window.1
                        ),
                    );
                }
            }
            WalkEnd::Abandoned(why) | WalkEnd::Refused(why) => {
                s.errors += 1;
                found.listing(&plan.dev.name, why);
            }
        }
    }
    // A stopped run did not reach every device, so it has no verdict on
    // which have gone quiet.
    if stop.requested() {
        return Ok(s);
    }
    let mut silent = Vec::new();
    for dev in &opts.sync.devices {
        let last: Option<i64> =
            sqlx::query_scalar("SELECT MAX(ts_ms) FROM yolink_readings WHERE device_name = ?")
                .bind(&dev.name)
                .fetch_one(db.pool())
                .await
                .with_context(|| format!("last reading of {}", dev.name))?;
        if let Some(detail) = silence(last, now_ms) {
            silent.push(SilentEntry {
                name: dev.name.clone(),
                detail,
            });
        }
    }
    found.silent(silent);
    Ok(s)
}

/// Why a device whose newest reading is `last_ts_ms` counts as gone
/// quiet at `now_ms`, or `None` while it is still reporting.
fn silence(last_ts_ms: Option<i64>, now_ms: i64) -> Option<String> {
    let Some(last) = last_ts_ms else {
        return Some("no readings yet; the device may be offline".into());
    };
    if now_ms - last <= SILENT_AFTER_MS {
        return None;
    }
    let since = Utc
        .timestamp_millis_opt(last)
        .single()
        .map_or_else(|| last.to_string(), |t| t.to_rfc3339());
    Some(format!(
        "no readings since {since}; the device may be offline"
    ))
}

/// Record the device and work out what of its range this run owes. The
/// inner `Err` is a device that cannot be walked; the outer, the store.
async fn plan_device<'a>(
    db: &RawDb,
    dev: &'a YolinkDevice,
    now_ms: i64,
    stride_ms: i64,
) -> Result<std::result::Result<Plan<'a>, String>> {
    let Some(start_ms) = start_of_day_ms(&dev.start) else {
        return Ok(Err(format!(
            "start {:?} is not a YYYY-MM-DD date",
            dev.start
        )));
    };
    let device_row = YolinkDeviceRow {
        id: dev.name.clone(),
        family_device_id: dev.family_device_id.clone(),
        kind: dev.kind.clone(),
        start_ms,
    };
    let now = datalib_time::IsoOffsetTimestamp::now_local();
    let mut tx = db.pool().begin().await?;
    bulk_upsert_in_tx(&mut tx, &[device_row], &now).await?;
    tx.commit().await?;

    let held = coverage::held(db.pool(), &device_scope(&dev.name)).await?;
    let mut windows = Vec::new();
    for gap in coverage::gaps(&wanted(start_ms, now_ms), &held) {
        windows.extend(windows_of(ms_of(&gap.lo)?, ms_of(&gap.hi)?, stride_ms));
    }
    Ok(Ok(Plan {
        dev,
        start_ms,
        windows,
    }))
}

/// A span end back to milliseconds. Every end in `coverage` under a
/// device scope was written by [`span_end`].
fn ms_of(end: &str) -> Result<i64> {
    end.parse()
        .with_context(|| format!("a coverage span end that is not milliseconds: {end:?}"))
}

/// How one device's walk ended.
#[derive(Debug, PartialEq, Eq)]
enum WalkEnd {
    Done,
    Stopped,
    /// Too many windows in a row failed; the rest of the walk is left
    /// for the next run.
    Abandoned(String),
    /// The device has no reading and YoLink refused its windows: most
    /// likely a wrong id, which no gap would say.
    Refused(String),
}

/// One device's walk: how it ended, and what it did not get.
struct Walked {
    end: WalkEnd,
    /// Each window that failed and stays a gap, with why.
    failed: Vec<(String, (i64, i64))>,
}

/// How one window came out.
enum WindowEnd {
    /// Fetched, or nothing a retry could fetch: covered either way.
    Covered,
    Failed(String),
    /// Refused (the status) while the device has no reading at all:
    /// left as a gap, since the next run may find the id was wrong or
    /// the device not yet deployed, and counted against the device.
    Refused(u16),
}

/// What the refusals of a device with no reading come to: `None` once it
/// has one, the refusals then being the time before it was deployed.
struct Refusals {
    asked: u32,
    refused: u32,
    status: Option<u16>,
}

impl Refusals {
    fn verdict(&self, first_reading: Option<i64>) -> Option<String> {
        let status = self.status.filter(|_| first_reading.is_none())?;
        Some(if self.refused == self.asked {
            format!("every window was refused (HTTP {status}); check the device id and its start")
        } else {
            format!(
                "{} of {} windows were refused (HTTP {status}) and the device has no reading \
                 yet; check the device id and its start",
                self.refused, self.asked
            )
        })
    }
}

struct Walk<'a, S> {
    db: &'a RawDb,
    src: &'a S,
    dev: &'a YolinkDevice,
    bar: &'a RunBar,
    stop: &'a datalib_etl::stop::StopFlag,
    overlap_ms: i64,
    start_ms: i64,
}

/// One more failed window in a row; `Some(why)` once the device has had
/// its budget.
fn failed_once(consecutive: &mut u32, window: (i64, i64)) -> Option<String> {
    *consecutive += 1;
    (*consecutive >= CONSECUTIVE_FAILURE_BUDGET).then(|| {
        format!(
            "abandoned after {consecutive} consecutive window failures \
             (last window {}..{}); the rest is walked next run",
            window.0, window.1
        )
    })
}

impl<S: WindowSource> Walk<'_, S> {
    async fn run(&self, windows: &[(i64, i64)], s: &mut FetchSummary) -> Result<Walked> {
        let dev = self.dev;
        info!(event = "yolink_begin", device = %dev.name, windows = windows.len(), "fetching one device");
        let mut walked = Walked {
            end: WalkEnd::Done,
            failed: Vec::new(),
        };
        let mut first_reading: Option<i64> =
            sqlx::query_scalar("SELECT MIN(ts_ms) FROM yolink_readings WHERE device_name = ?")
                .bind(&dev.name)
                .fetch_one(self.db.pool())
                .await?;
        let mut consecutive_failures: u32 = 0;
        let mut refusals = Refusals {
            asked: 0,
            refused: 0,
            status: None,
        };
        let mut refused_windows: Vec<(i64, i64)> = Vec::new();

        for &window in windows {
            if self.stop.requested() {
                walked.end = WalkEnd::Stopped;
                return Ok(walked);
            }
            refusals.asked += 1;
            match self.window(window, &mut first_reading, s).await? {
                WindowEnd::Covered => consecutive_failures = 0,
                WindowEnd::Failed(why) => {
                    if self.stop.requested() {
                        walked.end = WalkEnd::Stopped;
                        return Ok(walked);
                    }
                    s.windows_failed += 1;
                    walked.failed.push((why, window));
                    if let Some(why) = failed_once(&mut consecutive_failures, window) {
                        walked.end =
                            WalkEnd::Abandoned(refusals.verdict(first_reading).unwrap_or(why));
                        return Ok(walked);
                    }
                }
                WindowEnd::Refused(status) => {
                    refusals.refused += 1;
                    refusals.status = Some(status);
                    refused_windows.push(window);
                    if let Some(why) = failed_once(&mut consecutive_failures, window) {
                        walked.end =
                            WalkEnd::Abandoned(refusals.verdict(first_reading).unwrap_or(why));
                        return Ok(walked);
                    }
                }
            }
        }
        match (refusals.verdict(first_reading), first_reading) {
            (Some(why), _) => walked.end = WalkEnd::Refused(why),
            // The device has a reading now, so the windows refused before
            // it were the time before it was deployed: nothing there.
            (None, Some(first)) => {
                for window in refused_windows.into_iter().filter(|w| w.1 <= first) {
                    store_window(self.db.pool(), &dev.name, window, &[]).await?;
                }
            }
            (None, None) => {}
        }
        Ok(walked)
    }

    /// One window: fetched and covered, or left a gap. Only the store
    /// failing is an `Err`.
    async fn window(
        &self,
        window: (i64, i64),
        first_reading: &mut Option<i64>,
        s: &mut FetchSummary,
    ) -> Result<WindowEnd> {
        let dev = self.dev;
        let (ask_from, ask_to) = requested(window, self.overlap_ms, self.start_ms);
        let fetched = async {
            let body = self.src.csv(dev, ask_from, ask_to).await.context("fetch")?;
            parse(&body, &dev.kind).context("parse")
        }
        .await;
        s.requests += 1;
        s.windows += 1;
        self.bar.did(1);
        let rows = match fetched {
            Ok(rows) => rows,
            Err(e) => {
                if let Some(status) = refusal(&e).filter(|_| first_reading.is_none()) {
                    return Ok(WindowEnd::Refused(status));
                }
                let Some(why) = nothing_to_retry(&e, window.1, *first_reading) else {
                    return Ok(WindowEnd::Failed(format!("{e:#}")));
                };
                info!(event = "yolink_window_nothing_there", device = %dev.name, lo = window.0, hi = window.1, why, error = %format!("{e:#}"), "a window failed with nothing a retry could fetch");
                Vec::new()
            }
        };
        if let Some(earliest) = rows.iter().map(|r| r.ts_ms).min() {
            *first_reading = Some(first_reading.map_or(earliest, |f| f.min(earliest)));
        }
        let upserted = store_window(self.db.pool(), &dev.name, window, &rows).await?;
        s.readings += upserted;
        info!(event = "yolink_window", device = %dev.name, lo = window.0, hi = window.1, upserted, "fetched one window of a device's history");
        Ok(WindowEnd::Covered)
    }
}

/// The status YoLink refused a window with: a client error, not a
/// timeout or a rate limit.
fn refusal(e: &anyhow::Error) -> Option<u16> {
    e.downcast_ref::<HttpStatus>()
        .map(|HttpStatus(code)| *code)
        .filter(|c| (400..500).contains(c) && ![408, 429].contains(c))
}

/// Why a failed window of a device that has readings has nothing a retry
/// could fetch, or `None` when a retry might: YoLink said it is not there
/// (404, 410), or it refused a window ending before the device's first
/// reading, which is a configured `start` that predates the device.
fn nothing_to_retry(
    e: &anyhow::Error,
    end_ms: i64,
    first_reading: Option<i64>,
) -> Option<&'static str> {
    let status = refusal(e);
    if matches!(status, Some(404 | 410)) {
        return Some("not there upstream");
    }
    (status.is_some() && first_reading.is_some_and(|first| end_ms <= first))
        .then_some("before the device's first reading")
}

/// The request for one window's CSV. The signature is
/// `md5(family_device_id + start_ms + end_ms + device_udid)` —
/// reverse-engineered from the Safehous/YoLink Android Flutter
/// snapshot (see `ParamUtils::hashMD5` + `_THSensorNewChartScreenState`).
/// Yolink does not expose this scheme via its public API; UAC tokens
/// can't access historical data.
pub fn window_request(dev: &YolinkDevice, start_ms: i64, end_ms: i64) -> Result<HttpRequest> {
    let mut hasher = Md5::new();
    hasher.update(dev.family_device_id.as_bytes());
    hasher.update(start_ms.to_string().as_bytes());
    hasher.update(end_ms.to_string().as_bytes());
    hasher.update(dev.device_udid.as_bytes());
    let sig = hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect::<String>();

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
    Ok(HttpRequest::get(HttpService::Yolink, url).plain())
}

/// The HTTP status a request was refused with.
#[derive(Debug)]
pub(crate) struct HttpStatus(pub u16);

impl std::fmt::Display for HttpStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "HTTP {}", self.0)
    }
}

impl std::error::Error for HttpStatus {}

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

    /// An offline device's window comes back as an empty body, not even
    /// a header; that read as a missing `Time` column, a failure logged
    /// once per window per run.
    #[test]
    fn an_empty_body_is_no_readings() {
        assert!(parse("", "temperature_humidity").unwrap().is_empty());
        assert!(parse("\n", "watermeter").unwrap().is_empty());
    }

    #[test]
    fn a_device_is_silent_after_a_day_without_a_reading() {
        let day = SILENT_AFTER_MS;
        let now = 400 * day;
        assert_eq!(silence(Some(now - day), now), None);
        assert_eq!(
            silence(Some(0), now).as_deref(),
            Some("no readings since 1970-01-01T00:00:00+00:00; the device may be offline")
        );
        assert!(silence(None, now).is_some());
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

    /// A window's readings and its coverage land together; a window
    /// with no readings still counts as looked at.
    #[tokio::test]
    async fn a_window_lands_its_rows_and_its_coverage_in_one_transaction() {
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
        assert_eq!(
            store_window(pool, "v", (100, 300), &[r(100, 1.0), r(200, 2.0)])
                .await
                .unwrap(),
            2
        );
        // Re-upsert is idempotent on row count (dolt diff is the
        // authority on "did anything actually change?").
        assert_eq!(
            store_window(pool, "v", (100, 300), &[r(100, 1.5)])
                .await
                .unwrap(),
            1
        );
        assert_eq!(store_window(pool, "v", (300, 400), &[]).await.unwrap(), 0);
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM yolink_readings")
            .fetch_one(pool)
            .await
            .unwrap();
        assert_eq!(n, 2, "second upsert updates, doesn't duplicate");
        assert_eq!(
            coverage::held(pool, &device_scope("v")).await.unwrap(),
            [Span::new(span_end(100), span_end(400))],
            "the empty window is covered too"
        );
    }

    const HOUR: i64 = 3_600_000;

    #[test]
    fn a_gap_is_walked_in_strides_and_each_window_is_asked_with_its_overlap() {
        assert_eq!(windows_of(0, 0, HOUR), []);
        assert_eq!(windows_of(5, 0, HOUR), []);
        assert_eq!(windows_of(0, HOUR, HOUR), [(0, HOUR)]);
        assert_eq!(
            windows_of(0, 2 * HOUR + 1, HOUR),
            [(0, HOUR), (HOUR, 2 * HOUR), (2 * HOUR, 2 * HOUR + 1)]
        );
        assert_eq!(
            windows_of(0, 5, 0),
            [(0, 1), (1, 2), (2, 3), (3, 4), (4, 5)]
        );
        assert_eq!(
            requested((HOUR, 2 * HOUR), 60_000, 0),
            (HOUR - 60_000, 2 * HOUR)
        );
        assert_eq!(
            requested((HOUR, 2 * HOUR), 60_000, HOUR),
            (HOUR, 2 * HOUR),
            "never before the configured start"
        );
    }

    /// The range wanted begins at the configured start, or where
    /// YoLink's history begins if the start is older than that.
    #[test]
    fn the_range_wanted_stops_where_yolinks_history_does() {
        let now = 400 * 86_400_000;
        assert_eq!(
            wanted(now - 10 * 86_400_000, now),
            Span::new(span_end(now - 10 * 86_400_000), span_end(now))
        );
        assert_eq!(
            wanted(0, now),
            Span::new(span_end(now - HISTORY_KEPT_MS), span_end(now))
        );
    }

    #[test]
    fn a_start_is_the_utc_midnight_of_its_day() {
        assert_eq!(start_of_day_ms("1970-01-02"), Some(86_400_000));
        assert_eq!(start_of_day_ms("stardate 47457.1"), None);
    }
}

#[cfg(test)]
mod walk_tests {
    use super::*;
    use datalib_etl::stop::StopFlag;
    use std::collections::HashMap;
    use std::sync::Mutex;

    const DAY: i64 = 86_400_000;
    const DEVICE: &str = "warp-core-coolant";

    fn day(n: i64) -> i64 {
        Utc.with_ymd_and_hms(2369, 4, 1, 0, 0, 0)
            .unwrap()
            .timestamp_millis()
            + n * DAY
    }

    fn device(name: &str, start: &str) -> YolinkDevice {
        YolinkDevice {
            name: name.into(),
            kind: "watermeter".into(),
            start: start.into(),
            family_device_id: "0123456789abcdef0123456789abcdef".into(),
            device_udid: "fedcba9876543210fedcba9876543210".into(),
        }
    }

    fn sync(devices: Vec<YolinkDevice>, window_days: i64) -> YolinkSync {
        YolinkSync {
            overlap_minutes: None,
            window_days: Some(window_days),
            devices,
        }
    }

    /// A meter that reads once a day at noon from `deployed`, and a
    /// transport that fails the windows it is told to.
    struct Fake {
        deployed: i64,
        /// The meter's last reading is before this.
        until: i64,
        /// Request start → the status it is refused with, or `None` for
        /// a connection that drops.
        failing: Mutex<HashMap<i64, Option<u16>>>,
        fail_all: bool,
        asked: Mutex<Vec<(i64, i64)>>,
        stop_at: Option<(usize, StopFlag)>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                deployed: day(0),
                until: i64::MAX,
                failing: Mutex::new(HashMap::new()),
                fail_all: false,
                asked: Mutex::new(Vec::new()),
                stop_at: None,
            }
        }

        fn asked(&self) -> Vec<(i64, i64)> {
            std::mem::take(&mut *self.asked.lock().unwrap())
        }
    }

    impl WindowSource for Fake {
        async fn csv(&self, _dev: &YolinkDevice, start: i64, end: i64) -> Result<String> {
            let n = {
                let mut asked = self.asked.lock().unwrap();
                asked.push((start, end));
                asked.len()
            };
            if let Some((at, stop)) = &self.stop_at {
                if n >= *at {
                    stop.request();
                }
            }
            let failing = self.failing.lock().unwrap().get(&start).copied();
            if let Some(status) = failing.or(self.fail_all.then_some(None)) {
                return Err(match status {
                    Some(code) => anyhow::Error::new(HttpStatus(code)),
                    None => anyhow!("connection reset by peer"),
                });
            }
            let mut csv = String::from("Device Id,Time,Water Meter(GAL),Water Consumption(GAL)\n");
            let mut noon = self.deployed + DAY / 2;
            while noon < end.min(self.until) {
                if noon >= start {
                    let t = Utc.timestamp_millis_opt(noon).unwrap();
                    csv.push_str(&format!(
                        "d88b,{},{}.0,1.0\n",
                        t.format("%Y/%m/%d %H:%M:%S+0000"),
                        (noon - self.deployed) / DAY
                    ));
                }
                noon += DAY;
            }
            Ok(csv)
        }
    }

    struct Store {
        _dir: tempfile::TempDir,
        db: RawDb,
    }

    impl Store {
        async fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let db = RawDb::open(&dir.path().join("yl.doltlite_db"))
                .await
                .unwrap();
            Self { _dir: dir, db }
        }

        async fn run(&self, fake: &Fake, sync: &YolinkSync, now_ms: i64) -> FetchSummary {
            let stop = fake
                .stop_at
                .as_ref()
                .map_or_else(StopFlag::new, |(_, s)| s.clone());
            let opts = FetchOptions {
                db: self.db.clone(),
                sync: sync.clone(),
                now_ms,
                progress: Progress::noop(),
                control: DownloadControl {
                    stop,
                    ..Default::default()
                },
                sealer: None,
            };
            fetch_from(opts, fake).await.unwrap()
        }

        async fn problems(&self) -> Vec<(String, String)> {
            sqlx::query_as("SELECT scope_key, sample FROM problems ORDER BY scope_key")
                .fetch_all(self.db.pool())
                .await
                .unwrap()
        }

        async fn count(&self, sql: &'static str) -> i64 {
            sqlx::query_scalar(sql)
                .fetch_one(self.db.pool())
                .await
                .unwrap()
        }

        async fn held(&self, device: &str) -> Vec<(i64, i64)> {
            coverage::held(self.db.pool(), &device_scope(device))
                .await
                .unwrap()
                .iter()
                .map(|s| (ms_of(&s.lo).unwrap(), ms_of(&s.hi).unwrap()))
                .collect()
        }
    }

    fn keys(rows: &[(String, String)]) -> Vec<&str> {
        rows.iter().map(|r| r.0.as_str()).collect()
    }

    const FIVE_MIN: i64 = 300_000;

    /// A window that failed is a gap, asked for again next run while
    /// the rest of the device's range is held. YoLink forgets history
    /// after about two months, so that it is asked again at all is what
    /// matters.
    #[tokio::test]
    async fn a_failed_window_is_a_gap_and_is_fetched_again() {
        let st = Store::new().await;
        let cfg = sync(vec![device(DEVICE, "2369-04-01")], 7);
        let fake = Fake::new();
        fake.failing.lock().unwrap().insert(day(7) - FIVE_MIN, None);
        let s = st.run(&fake, &cfg, day(28)).await;
        assert_eq!((s.windows, s.windows_failed, s.errors), (4, 1, 0), "{s:?}");
        let rows = st.problems().await;
        assert_eq!(keys(&rows), [format!("listing:{DEVICE}").as_str()]);
        assert!(rows[0].1.contains("1 of 4 windows"), "{rows:?}");
        assert_eq!(
            st.count("SELECT COUNT(DISTINCT ts_ms) FROM yolink_readings")
                .await,
            21,
            "the failed week is missing"
        );
        assert_eq!(
            st.held(DEVICE).await,
            [(day(0), day(7)), (day(14), day(28))],
            "the failed window is the gap"
        );
        fake.asked();

        fake.failing.lock().unwrap().clear();
        let s = st.run(&fake, &cfg, day(29)).await;
        assert_eq!((s.windows_failed, s.errors), (0, 0), "{s:?}");
        assert_eq!(
            fake.asked(),
            [(day(7) - FIVE_MIN, day(14)), (day(28) - FIVE_MIN, day(29))],
            "the gap, then the day since the last run, each with its overlap"
        );
        assert!(st.problems().await.is_empty(), "{:?}", st.problems().await);
        assert_eq!(
            st.count("SELECT COUNT(DISTINCT ts_ms) FROM yolink_readings")
                .await,
            29
        );
        assert_eq!(st.held(DEVICE).await, [(day(0), day(29))]);
    }

    /// A configured `start` before the device existed is refused; that
    /// window has nothing to retry, is covered, and is no problem. A
    /// refusal once the device has readings is, until a retry says the
    /// window is not there at all.
    #[tokio::test]
    async fn a_refusal_before_the_first_reading_is_not_a_failure() {
        let st = Store::new().await;
        let cfg = sync(vec![device(DEVICE, "2369-04-01")], 7);
        let mut fake = Fake::new();
        fake.deployed = day(7);
        fake.failing.lock().unwrap().insert(day(0), Some(403));
        fake.failing
            .lock()
            .unwrap()
            .insert(day(14) - FIVE_MIN, Some(403));
        let s = st.run(&fake, &cfg, day(28)).await;
        assert_eq!((s.windows_failed, s.errors), (1, 0), "{s:?}");
        let rows = st.problems().await;
        assert_eq!(keys(&rows), [format!("listing:{DEVICE}").as_str()]);
        assert!(rows[0].1.contains("HTTP 403"), "{rows:?}");
        assert_eq!(
            st.held(DEVICE).await,
            [(day(0), day(14)), (day(21), day(28))],
            "the window refused before the first reading is covered; the one refused \
             after it is a gap"
        );

        fake.failing
            .lock()
            .unwrap()
            .insert(day(14) - FIVE_MIN, Some(404));
        let s = st.run(&fake, &cfg, day(29)).await;
        assert_eq!(s.windows_failed, 0, "{s:?}");
        assert!(st.problems().await.is_empty(), "{:?}", st.problems().await);
        assert_eq!(st.held(DEVICE).await, [(day(0), day(29))]);
    }

    /// A device with no reading at all whose every window is refused is
    /// most likely a wrong id; reading that as "not deployed yet" said
    /// nothing, run after run. It is a row on the device and its windows
    /// stay gaps; the row goes on the run that first gets a reading, the
    /// earlier refusals then being the time before it was deployed.
    #[tokio::test]
    async fn a_device_whose_every_window_is_refused_is_a_listing_row() {
        let st = Store::new().await;
        let cfg = sync(vec![device(DEVICE, "2369-04-01")], 7);
        let mut fake = Fake::new();
        fake.deployed = day(21);
        fake.failing.lock().unwrap().insert(day(0), Some(403));
        for d in [7, 14, 21] {
            fake.failing
                .lock()
                .unwrap()
                .insert(day(d) - FIVE_MIN, Some(403));
        }
        let s = st.run(&fake, &cfg, day(28)).await;
        assert_eq!((s.windows, s.errors), (4, 1), "{s:?}");
        let rows = st.problems().await;
        let listing = format!("listing:{DEVICE}");
        assert_eq!(
            keys(&rows),
            [listing.as_str(), &format!("silent:{DEVICE}")],
            "{rows:?}"
        );
        assert!(rows[0].1.contains("HTTP 403"), "{rows:?}");
        assert!(st.held(DEVICE).await.is_empty(), "nothing was looked at");

        fake.failing.lock().unwrap().remove(&(day(21) - FIVE_MIN));
        let s = st.run(&fake, &cfg, day(28)).await;
        assert_eq!(s.errors, 0, "{s:?}");
        assert!(st.problems().await.is_empty(), "{:?}", st.problems().await);
        assert_eq!(st.held(DEVICE).await, [(day(0), day(28))]);
    }

    /// Nothing older than the history YoLink keeps is asked for, and a
    /// device the config stopped naming is not walked.
    #[tokio::test]
    async fn only_the_history_yolink_keeps_is_wanted() {
        let st = Store::new().await;
        let fake = Fake::new();
        let both = sync(
            vec![
                device(DEVICE, "2369-04-01"),
                device("cargo-bay-2", "2369-04-01"),
            ],
            7,
        );
        st.run(&fake, &both, day(100)).await;
        let asked = fake.asked();
        let horizon = day(100) - HISTORY_KEPT_MS;
        assert!(
            asked.iter().all(|(s, _)| *s >= horizon - FIVE_MIN),
            "{asked:?}"
        );
        assert_eq!(asked.len(), 2 * 10, "ten windows a device for 66 days");
        assert_eq!(st.held(DEVICE).await, [(horizon, day(100))]);

        let one = sync(vec![device(DEVICE, "2369-04-01")], 7);
        st.run(&fake, &one, day(101)).await;
        assert_eq!(fake.asked().len(), 1, "the one device, the one day");
        assert_eq!(st.held("cargo-bay-2").await, [(horizon, day(100))]);
    }

    /// A device whose `start` is not a date cannot be walked; it costs
    /// that device, as a row, not only a log line.
    #[tokio::test]
    async fn a_device_that_cannot_be_planned_is_a_listing_row() {
        let st = Store::new().await;
        let cfg = sync(
            vec![
                device("holodeck-3", "stardate 47457.1"),
                device(DEVICE, "2369-04-01"),
            ],
            7,
        );
        let s = st.run(&Fake::new(), &cfg, day(28)).await;
        assert_eq!(s.errors, 1, "{s:?}");
        assert_eq!(
            st.count("SELECT COUNT(DISTINCT ts_ms) FROM yolink_readings")
                .await,
            28
        );
        let rows = st.problems().await;
        assert!(keys(&rows).contains(&"listing:holodeck-3"), "{rows:?}");
    }

    /// Thirty failures in a row abandon the device for the run, as a
    /// row. The windows it failed are gaps, so the next run asks for
    /// them again and the row goes once it has.
    #[tokio::test]
    async fn an_abandoned_device_is_a_listing_row_and_its_gaps_are_walked_next_run() {
        let st = Store::new().await;
        let cfg = sync(vec![device(DEVICE, "2369-04-01")], 1);
        let mut fake = Fake::new();
        fake.fail_all = true;
        let s = st.run(&fake, &cfg, day(40)).await;
        assert_eq!((s.windows, s.errors), (30, 1), "{s:?}");
        let rows = st.problems().await;
        let listing = rows
            .iter()
            .find(|r| r.0 == format!("listing:{DEVICE}"))
            .unwrap_or_else(|| panic!("{rows:?}"));
        assert!(listing.1.contains("abandoned after 30"), "{rows:?}");
        assert!(st.held(DEVICE).await.is_empty());

        fake.fail_all = false;
        let s = st.run(&fake, &cfg, day(40)).await;
        assert_eq!((s.windows, s.windows_failed, s.errors), (40, 0, 0), "{s:?}");
        assert!(st.problems().await.is_empty(), "{:?}", st.problems().await);
        assert_eq!(st.held(DEVICE).await, [(day(0), day(40))]);
    }

    /// A stop ends the walk at the next window, and a stopped run says
    /// nothing about the devices it did not reach: here the device would
    /// read as gone quiet, its newest reading weeks before the run.
    #[tokio::test]
    async fn a_stop_ends_the_walk_and_leaves_the_run_level_rows() {
        let st = Store::new().await;
        let cfg = sync(vec![device(DEVICE, "2369-04-01")], 7);
        let mut fake = Fake::new();
        fake.stop_at = Some((1, StopFlag::new()));
        let s = st.run(&fake, &cfg, day(28)).await;
        assert_eq!(s.windows, 1, "{s:?}");
        assert!(st.problems().await.is_empty(), "{:?}", st.problems().await);
        assert_eq!(
            st.held(DEVICE).await,
            [(day(0), day(7))],
            "the next run owes the rest"
        );
    }

    /// Y1: a device that stops reporting answers every window after its
    /// last reading with an empty body. Nothing recorded that those
    /// windows were looked at, so each run asked for everything from the
    /// last reading to now again, a stretch that grew by the day.
    #[tokio::test]
    async fn a_silent_device_is_not_asked_for_its_silence_again() {
        let st = Store::new().await;
        let cfg = sync(vec![device(DEVICE, "2369-04-01")], 7);
        let mut fake = Fake::new();
        fake.until = day(10);
        st.run(&fake, &cfg, day(28)).await;
        assert_eq!(fake.asked().len(), 4);

        let s = st.run(&fake, &cfg, day(29)).await;
        let asked = fake.asked();
        assert_eq!(
            asked.len(),
            1,
            "only the day since the last run is asked for, not the silence before it: {asked:?}"
        );
        assert!(
            asked[0].0 >= day(28) - FIVE_MIN,
            "the one window starts at the last run's end, less the overlap: {asked:?}"
        );
        assert_eq!((s.windows, s.errors), (1, 0), "{s:?}");
    }

    /// A widened `start` is owed below what was walked, once. When one
    /// other device could not be walked, the record of the widening was
    /// never written, so every widened device re-walked from its start
    /// on every run after.
    #[tokio::test]
    async fn a_widened_start_is_walked_once_whatever_another_device_does() {
        let st = Store::new().await;
        let fake = Fake::new();
        let narrow = sync(
            vec![
                device(DEVICE, "2369-04-15"),
                device("cargo-bay-2", "2369-04-15"),
            ],
            7,
        );
        st.run(&fake, &narrow, day(28)).await;
        fake.asked();

        let wide = sync(
            vec![
                device(DEVICE, "2369-04-01"),
                device("cargo-bay-2", "2369-04-01"),
                device("holodeck-3", "stardate 47457.1"),
            ],
            7,
        );
        let s = st.run(&fake, &wide, day(28)).await;
        assert_eq!(s.errors, 1, "{s:?}");
        let asked = fake.asked();
        assert_eq!(
            asked.iter().filter(|(s, _)| *s == day(0)).count(),
            2,
            "both widened devices are walked from the new start: {asked:?}"
        );

        let s = st.run(&fake, &wide, day(29)).await;
        assert_eq!(s.errors, 1, "{s:?}");
        let asked = fake.asked();
        assert!(
            !asked.iter().any(|(s, _)| *s == day(0)),
            "the widened stretch was walked last run; nothing asks for it again: {asked:?}"
        );
    }

    /// A refused window, however the status is carried, is told from a
    /// dropped connection; a 404 is nothing to retry.
    #[test]
    fn a_refusal_is_a_client_error_that_is_not_a_timeout_or_a_rate_limit() {
        let status = |code| anyhow::Error::new(HttpStatus(code)).context("fetch");
        assert_eq!(refusal(&status(403)), Some(403));
        assert_eq!(refusal(&status(429)), None);
        assert_eq!(refusal(&status(503)), None);
        assert_eq!(refusal(&anyhow!("connection reset by peer")), None);
        assert_eq!(
            nothing_to_retry(&status(404), 10, None),
            Some("not there upstream")
        );
        assert_eq!(
            nothing_to_retry(&status(403), 10, Some(20)),
            Some("before the device's first reading")
        );
        assert_eq!(nothing_to_retry(&status(403), 30, Some(20)), None);
    }
}
