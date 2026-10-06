//! Yolink download → doltlite. For-loop over devices, inner loop
//! over forward-walking time windows: curl, parse, bulk-upsert. No
//! per-window `dolt_commit`: the sync orchestrator wraps the whole
//! download in one commit when [`fetch`] returns, which is the right
//! grain (a sync run is a single "snapshot of upstream"). `dolt
//! diff` against that trailing commit shows exactly which readings
//! moved this run — same source-of-truth pattern every other
//! provider uses.

pub mod schema_raw;

use std::collections::HashSet;
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
use datalib_etl::download_problems::SilentEntry;
use datalib_etl::progress::{Progress, RunBar};
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl_yolink_config::{YolinkDevice, YolinkSync};

use schema_raw::{
    full_ddl, window_id_recipe, YolinkDeviceRow, YolinkReadingRow, YOLINK_WINDOWS_TABLE,
};

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
/// A device with no reading this recent is reported as gone quiet. The
/// sensors report every few minutes, so a day is far past a gap.
const SILENT_AFTER_MS: i64 = 86_400_000;
/// How far back YoLink still serves history: about 66 days on the one
/// account measured (see INGEST.md). A failed window that ends before
/// this is not retried, since the readings it held are gone upstream.
const HISTORY_KEPT_MS: i64 = 66 * 86_400_000;
/// Failed windows in a row, retried or walked, before a device is left
/// for the next run: a run this long is a stuck credential or a dead
/// device, and each more is a request for nothing.
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
    full_ddl()
);

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
    let now = datalib_time::IsoOffsetTimestamp::now_local();
    let mut tx = pool.begin().await?;
    bulk_upsert_in_tx(&mut tx, &rows, &now).await?;
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
    /// Windows that failed this run; each is a `yolink_windows` row.
    pub windows_failed: usize,
    pub requests: usize,
}

/// Scope key for this provider's [`datalib_etl::scope_config`] blob.
const SCOPE_CONFIG_KEY: &str = "yolink:download";

/// Blob key. Named so writer and reader can't drift.
const K_DEVICE_STARTS: &str = "device_starts";

fn scope_config_blob(sync: &YolinkSync) -> serde_json::Value {
    let starts: std::collections::BTreeMap<&str, &str> = sync
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

/// Where one window's CSV comes from: YoLink over `curl` in a run, canned
/// answers in a test.
pub(crate) trait WindowSource {
    async fn csv(&self, dev: &YolinkDevice, start_ms: i64, end_ms: i64) -> Result<String>;
}

struct Curl;

impl WindowSource for Curl {
    async fn csv(&self, dev: &YolinkDevice, start_ms: i64, end_ms: i64) -> Result<String> {
        curl(&build_signed_url(dev, start_ms, end_ms)?).await
    }
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    fetch_from(opts, &Curl).await
}

/// One device's share of the run: where its forward walk begins, and the
/// windows an earlier run failed.
struct Plan<'a> {
    dev: &'a YolinkDevice,
    cursor: i64,
    /// Failed windows that start behind the cursor: asked for again
    /// before the forward walk.
    retry: Vec<(i64, i64)>,
    /// Failed windows the forward walk will cover again. Dropped once it
    /// has, unless it failed them again.
    ahead: Vec<String>,
}

pub(crate) async fn fetch_from<S: WindowSource>(
    opts: FetchOptions,
    src: &S,
) -> Result<FetchSummary> {
    let (pool, stop) = (opts.db.pool().clone(), opts.control.stop.clone());
    run_problems::collecting(&pool, &stop, |found| walk_devices(opts, src, found)).await
}

async fn walk_devices<S: WindowSource>(
    opts: FetchOptions,
    src: &S,
    found: RunProblems,
) -> Result<FetchSummary> {
    // Built before `opts.db` is moved out below.
    let scope_cfg = scope_config_blob(&opts.sync);
    let db = opts.db;
    let stop = &opts.control.stop;
    let overlap_ms = opts.sync.overlap_minutes.unwrap_or(DEFAULT_OVERLAP_MINUTES) * 60_000;
    let stride_ms = opts.sync.window_days.unwrap_or(DEFAULT_WINDOW_DAYS) * 86_400_000;
    let window_ms = stride_ms.saturating_add(overlap_ms);
    let mut s = FetchSummary {
        devices: opts.sync.devices.len(),
        ..Default::default()
    };
    let now_ms = opts.now_ms;
    // Diff the per-device `start` dates against the ones that produced
    // the stored resume cursors. `None` (fresh store, or one written before
    // `sync_scope_config` existed) plans no backfill.
    let prior_scope_cfg =
        datalib_etl::scope_config::load_or_none(db.pool(), SCOPE_CONFIG_KEY).await;

    let horizon_ms = now_ms - HISTORY_KEPT_MS;
    retire_windows(db.pool(), &opts.sync.devices, horizon_ms).await?;

    // Every device's resume point first, so the bar counts requests (one
    // per window) rather than devices, which finish in uneven lumps.
    let mut plans = Vec::with_capacity(opts.sync.devices.len());
    for dev in &opts.sync.devices {
        let prior_start = prior_start_for(prior_scope_cfg.as_ref(), &dev.name);
        match plan_device(&db, dev, prior_start.as_deref(), overlap_ms).await? {
            Ok(cursor) => {
                let (retry, ahead): (Vec<_>, Vec<_>) = failed_windows(db.pool(), &dev.name)
                    .await?
                    .into_iter()
                    .partition(|(_, start, _)| *start < cursor);
                plans.push(Plan {
                    dev,
                    cursor,
                    retry: retry.into_iter().map(|(_, s, e)| (s, e)).collect(),
                    ahead: ahead.into_iter().map(|(id, _, _)| id).collect(),
                });
            }
            Err(why) => {
                s.errors += 1;
                found.listing(&dev.name, why);
            }
        }
    }
    let requests: u64 = plans
        .iter()
        .map(|p| p.retry.len() as u64 + window_count(p.cursor, now_ms, stride_ms))
        .sum();
    let bar = RunBar::new(&opts.progress, requests);

    for plan in &plans {
        bar.doing(&format!("yolink: {}", plan.dev.name));
        let walk = Walk {
            db: &db,
            src,
            dev: plan.dev,
            bar: &bar,
            stop,
            horizon_ms,
        };
        match walk.run(plan, stride_ms, window_ms, now_ms, &mut s).await? {
            WalkEnd::Done | WalkEnd::Stopped => {}
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
    // Record the config only when every device succeeded: a device that
    // errored hasn't covered its widened `start`, and the blob is one
    // row for all of them.
    datalib_etl::scope_config::store_if_satisfied(
        db.pool(),
        SCOPE_CONFIG_KEY,
        &scope_cfg,
        s.errors == 0,
    )
    .await;
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

/// How many windows the forward walk requests from `cursor` to
/// `now_ms`: one per stride, the last one cut short at `now_ms`.
fn window_count(cursor: i64, now_ms: i64, stride_ms: i64) -> u64 {
    if cursor >= now_ms {
        return 0;
    }
    let stride = stride_ms.max(1) as u128;
    let span = (now_ms as i128 - cursor as i128) as u128;
    span.div_ceil(stride) as u64
}

/// What the resume decision did, for the caller to log.
#[derive(Debug, PartialEq, Eq)]
enum CursorNote {
    /// Resumed from the resume cursor (clamped forward to `start`, which is
    /// a floor). The ordinary case.
    Normal,
    /// `start` moved earlier than the run that produced the resume cursor,
    /// so the range below the old start was never walked. Reset to the
    /// new start; windows are UPSERT-deduped, so re-walking the overlap
    /// costs requests, not correctness.
    Backfill,
    /// `start` moved later, past the resume cursor. The clamp jumps the
    /// cursor forward and `[resume cursor, start]` is never fetched. That is
    /// what `start` literally asks for, so it's preserved — but said out
    /// loud rather than done silently.
    SkipsAhead,
}

/// Where this run should begin walking for one device.
///
/// Pure so the config-change branches are testable without a transport.
fn resume_cursor(
    stored_ms: Option<i64>,
    start_ms: i64,
    overlap_ms: i64,
    start_widened: bool,
    start_narrowed: bool,
) -> (i64, CursorNote) {
    match stored_ms {
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

/// Record the device and decide where this run's walk of it begins. The
/// inner `Err` is a device that cannot be walked; the outer, the store.
async fn plan_device(
    db: &RawDb,
    dev: &YolinkDevice,
    prior_start: Option<&str>,
    overlap_ms: i64,
) -> Result<std::result::Result<i64, String>> {
    let start_ms = match NaiveDate::parse_from_str(&dev.start, "%Y-%m-%d") {
        Ok(d) => d
            .and_hms_opt(0, 0, 0)
            .map(|dt| Utc.from_utc_datetime(&dt).timestamp_millis())
            .unwrap(),
        Err(e) => return Ok(Err(format!("start {:?}: {e}", dev.start))),
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

    let stored_ms: Option<i64> =
        sqlx::query_scalar("SELECT last_ts_ms FROM yolink_devices WHERE id = ?")
            .bind(&dev.name)
            .fetch_one(db.pool())
            .await?;

    // Only a *recorded* move counts. Comparing `start` to the resume cursor
    // alone would fire on every run of any store whose configured start
    // simply sits ahead of its data. `YYYY-MM-DD` sorts lexicographically
    // as it does chronologically (validated at config load).
    let start_widened = prior_start.is_some_and(|p| dev.start.as_str() < p);
    let start_narrowed = prior_start.is_some_and(|p| dev.start.as_str() > p);

    let (cursor_start, note) = resume_cursor(
        stored_ms,
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
            "start moved past the stored resume cursor; the range between \
             them will not be fetched",
        ),
        CursorNote::Normal => {}
    }
    Ok(Ok(cursor_start))
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
    /// likely a wrong id, which no window row would say.
    Refused(String),
}

/// How one window came out.
enum WindowEnd {
    Fetched,
    Failed,
    /// Nothing a retry could fetch; see [`nothing_to_retry`].
    NothingThere,
    /// Refused (the status) while the device has no reading at all:
    /// not a window row, which would be retried for ever, but the
    /// device's own row if nothing else comes back.
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
    /// Where YoLink's history begins, as far as this run knows.
    horizon_ms: i64,
}

/// One more failed window in a row; `Some(why)` once the device has had
/// its budget.
fn failed_once(consecutive: &mut u32, start: i64, end: i64) -> Option<String> {
    *consecutive += 1;
    (*consecutive >= CONSECUTIVE_FAILURE_BUDGET).then(|| {
        format!(
            "abandoned after {consecutive} consecutive window failures \
             (last window {start}..{end}); the rest is walked next run"
        )
    })
}

impl<S: WindowSource> Walk<'_, S> {
    async fn run(
        &self,
        plan: &Plan<'_>,
        stride_ms: i64,
        window_ms: i64,
        now_ms: i64,
        s: &mut FetchSummary,
    ) -> Result<WalkEnd> {
        let dev = self.dev;
        info!(event = "yolink_begin", device = %dev.name, cursor = plan.cursor, retry = plan.retry.len(), now_ms, "fetching one device");
        let mut first_reading: Option<i64> =
            sqlx::query_scalar("SELECT MIN(ts_ms) FROM yolink_readings WHERE device_name = ?")
                .bind(&dev.name)
                .fetch_one(self.db.pool())
                .await?;
        let mut failed_now: HashSet<String> = HashSet::new();
        let mut consecutive_failures: u32 = 0;
        let mut refusals = Refusals {
            asked: 0,
            refused: 0,
            status: None,
        };

        for &(start, end) in &plan.retry {
            if self.stop.requested() {
                return self.finish(WalkEnd::Stopped).await;
            }
            refusals.asked += 1;
            match self.window(start, end, &mut first_reading, s).await? {
                WindowEnd::Failed => {
                    failed_now.insert(window_id_recipe(&dev.name, start, end));
                    if let Some(why) = failed_once(&mut consecutive_failures, start, end) {
                        let why = refusals.verdict(first_reading).unwrap_or(why);
                        return self.finish(WalkEnd::Abandoned(why)).await;
                    }
                }
                WindowEnd::Refused(status) => {
                    refusals.refused += 1;
                    refusals.status = Some(status);
                    forget_window(self.db.pool(), &window_id_recipe(&dev.name, start, end)).await?;
                    if let Some(why) = failed_once(&mut consecutive_failures, start, end) {
                        let why = refusals.verdict(first_reading).unwrap_or(why);
                        return self.finish(WalkEnd::Abandoned(why)).await;
                    }
                }
                WindowEnd::Fetched | WindowEnd::NothingThere => {
                    consecutive_failures = 0;
                    forget_window(self.db.pool(), &window_id_recipe(&dev.name, start, end)).await?;
                }
            }
        }

        let ahead: HashSet<&str> = plan.ahead.iter().map(String::as_str).collect();
        let mut cursor = plan.cursor;
        while cursor < now_ms {
            if self.stop.requested() {
                return self.finish(WalkEnd::Stopped).await;
            }
            let end = cursor.saturating_add(window_ms).min(now_ms);
            let id = window_id_recipe(&dev.name, cursor, end);
            refusals.asked += 1;
            match self.window(cursor, end, &mut first_reading, s).await? {
                WindowEnd::Failed => {
                    failed_now.insert(id);
                    if let Some(why) = failed_once(&mut consecutive_failures, cursor, end) {
                        let why = refusals.verdict(first_reading).unwrap_or(why);
                        return self.finish(WalkEnd::Abandoned(why)).await;
                    }
                }
                WindowEnd::Refused(status) => {
                    refusals.refused += 1;
                    refusals.status = Some(status);
                    if let Some(why) = failed_once(&mut consecutive_failures, cursor, end) {
                        let why = refusals.verdict(first_reading).unwrap_or(why);
                        return self.finish(WalkEnd::Abandoned(why)).await;
                    }
                }
                WindowEnd::Fetched | WindowEnd::NothingThere => {
                    consecutive_failures = 0;
                }
            }
            cursor = cursor.saturating_add(stride_ms).max(cursor + 1);
        }
        // The walk reached now, so every failed window ahead of its start
        // was asked for again; the ones that did not fail again are done.
        for id in ahead {
            if !failed_now.contains(id) {
                forget_window(self.db.pool(), id).await?;
            }
        }
        match refusals.verdict(first_reading) {
            Some(why) => self.finish(WalkEnd::Refused(why)).await,
            None => self.finish(WalkEnd::Done).await,
        }
    }

    /// One window: fetched and written, or recorded as failed. Only the
    /// store failing is an `Err`.
    async fn window(
        &self,
        start: i64,
        end: i64,
        first_reading: &mut Option<i64>,
        s: &mut FetchSummary,
    ) -> Result<WindowEnd> {
        let dev = self.dev;
        let fetched = async {
            let body = self.src.csv(dev, start, end).await.context("curl")?;
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
                if let Some(why) = nothing_to_retry(&e, end, *first_reading, self.horizon_ms) {
                    info!(event = "yolink_window_nothing_there", device = %dev.name, start, end, why, error = %format!("{e:#}"), "a window failed with nothing a retry could fetch");
                    return Ok(WindowEnd::NothingThere);
                }
                s.windows_failed += 1;
                let err = format!("{e:#}");
                record_window_failure(self.db.pool(), &dev.name, start, end, &err).await?;
                return Ok(WindowEnd::Failed);
            }
        };
        if let Some(earliest) = rows.iter().map(|r| r.ts_ms).min() {
            *first_reading = Some(first_reading.map_or(earliest, |f| f.min(earliest)));
        }
        let upserted = upsert_readings(self.db.pool(), &dev.name, &rows).await?;
        s.readings += upserted;
        info!(event = "yolink_window", device = %dev.name, start, end, upserted, "fetched one window of a device's history");
        Ok(WindowEnd::Fetched)
    }

    /// The device's resume point is its newest reading, however the walk
    /// ended: what landed is behind it, what did not is a window row.
    async fn finish(&self, end: WalkEnd) -> Result<WalkEnd> {
        sqlx::query(
            "UPDATE yolink_devices SET last_ts_ms =
                (SELECT MAX(ts_ms) FROM yolink_readings WHERE device_name = ?)
             WHERE id = ?",
        )
        .bind(&self.dev.name)
        .bind(&self.dev.name)
        .execute(self.db.pool())
        .await?;
        Ok(end)
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
/// (404, 410); it refused a window ending before the device's first
/// reading, which is a configured `start` that predates the device; or
/// the window ends before the history YoLink keeps.
fn nothing_to_retry(
    e: &anyhow::Error,
    end_ms: i64,
    first_reading: Option<i64>,
    horizon_ms: i64,
) -> Option<&'static str> {
    let status = refusal(e);
    if matches!(status, Some(404 | 410)) {
        return Some("not there upstream");
    }
    if status.is_some() && first_reading.is_some_and(|first| end_ms <= first) {
        return Some("before the device's first reading");
    }
    (end_ms <= horizon_ms).then_some("older than the history YoLink keeps")
}

/// Drop the failed windows no run will ask for again: those of a device
/// the config no longer names, and those that end before the history
/// YoLink keeps.
async fn retire_windows(
    pool: &SqlitePool,
    devices: &[YolinkDevice],
    horizon_ms: i64,
) -> Result<()> {
    let configured: HashSet<&str> = devices.iter().map(|d| d.name.as_str()).collect();
    let rows: Vec<(String, String, i64)> =
        sqlx::query_as("SELECT id, device_name, end_ms FROM yolink_windows")
            .fetch_all(pool)
            .await
            .context("list yolink_windows")?;
    let keep: HashSet<String> = rows
        .into_iter()
        .filter(|(_, device, end)| configured.contains(device.as_str()) && *end > horizon_ms)
        .map(|(id, _, _)| id)
        .collect();
    let gone = datalib_etl::prune::prune_scope(pool, YOLINK_WINDOWS_TABLE, &[], &keep).await?;
    if !gone.is_empty() {
        info!(
            event = "yolink_windows_retired",
            windows = gone.len(),
            "failed windows no run will ask for again"
        );
    }
    Ok(())
}

/// Every failed window of one device: `(id, start_ms, end_ms)`.
async fn failed_windows(pool: &SqlitePool, device: &str) -> Result<Vec<(String, i64, i64)>> {
    sqlx::query_as(
        "SELECT id, start_ms, end_ms FROM yolink_windows WHERE device_name = ? ORDER BY start_ms",
    )
    .bind(device)
    .fetch_all(pool)
    .await
    .with_context(|| format!("failed windows of {device}"))
}

async fn record_window_failure(
    pool: &SqlitePool,
    device: &str,
    start_ms: i64,
    end_ms: i64,
    err: &str,
) -> Result<()> {
    let id = window_id_recipe(device, start_ms, end_ms);
    let mut tx = pool.begin().await?;
    sqlx::query(
        "INSERT INTO yolink_windows (id, device_name, start_ms, end_ms) VALUES (?, ?, ?, ?) \
         ON CONFLICT(id) DO NOTHING",
    )
    .bind(&id)
    .bind(device)
    .bind(start_ms)
    .bind(end_ms)
    .execute(&mut *tx)
    .await?;
    dr::record_object_error(&mut tx, YOLINK_WINDOWS_TABLE, &id, err).await?;
    tx.commit().await?;
    Ok(())
}

/// A window that fetched, or that never will: its row, sidecar and
/// problem go.
async fn forget_window(pool: &SqlitePool, id: &str) -> Result<()> {
    datalib_etl::prune::prune_scope(pool, YOLINK_WINDOWS_TABLE, &[("id", id)], &HashSet::new())
        .await?;
    Ok(())
}

/// Compose and sign the per-window CSV download URL. The signature
/// is `md5(family_device_id + start_ms + end_ms + device_udid)` —
/// reverse-engineered from the Safehous/YoLink Android Flutter
/// snapshot (see `ParamUtils::hashMD5` + `_THSensorNewChartScreenState`).
/// Yolink does not expose this scheme via its public API; UAC tokens
/// can't access historical data.
fn build_signed_url(dev: &YolinkDevice, start_ms: i64, end_ms: i64) -> Result<String> {
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
    Ok(url)
}

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
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let e = anyhow!("curl exit {}: {}", out.status, stderr.trim());
        return Err(match http_status_of(&stderr) {
            Some(code) => anyhow::Error::new(HttpStatus(code)).context(e.to_string()),
            None => e,
        });
    }
    String::from_utf8(out.stdout).context("response not UTF-8")
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

/// `curl -f`'s "The requested URL returned error: 404" → 404.
fn http_status_of(stderr: &str) -> Option<u16> {
    let (_, rest) = stderr.split_once("returned error: ")?;
    rest.get(..3)?.parse().ok()
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

    fn sync_with(devices: Vec<YolinkDevice>) -> YolinkSync {
        YolinkSync {
            overlap_minutes: None,
            window_days: None,
            devices,
        }
    }

    #[test]
    fn blob_records_starts_keyed_by_device_name() {
        let blob = scope_config_blob(&sync_with(vec![dev("freezer", "2024-01-01")]));
        assert_eq!(blob, json!({"device_starts": {"freezer": "2024-01-01"}}));
    }

    #[test]
    fn blob_omits_pagination_knobs() {
        // `overlap_minutes` / `window_days` are re-applied every run, so
        // recording them would only provoke pointless re-walks.
        let mut o = sync_with(vec![dev("freezer", "2024-01-01")]);
        o.overlap_minutes = Some(99);
        o.window_days = Some(3);
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

    // ── resume cursor ────────────────────────────────────────────────

    const HOUR: i64 = 3_600_000;

    #[test]
    fn cold_start_begins_at_start() {
        assert_eq!(
            resume_cursor(None, 1_000, 60_000, false, false),
            (1_000, CursorNote::Normal)
        );
    }

    #[test]
    fn resume_cursor_resumes_with_overlap() {
        let (c, note) = resume_cursor(Some(100 * HOUR), HOUR, HOUR, false, false);
        assert_eq!(c, 99 * HOUR, "one overlap back from the resume cursor");
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
        // The whole point: a resume cursor far ahead does not suppress the
        // backfill when `start` moved earlier.
        let (c, note) = resume_cursor(Some(100 * HOUR), 2 * HOUR, HOUR, true, false);
        assert_eq!(c, 2 * HOUR);
        assert_eq!(note, CursorNote::Backfill);
    }

    #[test]
    fn narrowed_start_past_the_resume_cursor_is_flagged() {
        let (c, note) = resume_cursor(Some(10 * HOUR), 50 * HOUR, HOUR, false, true);
        assert_eq!(c, 50 * HOUR, "start wins; the gap is what it asks for");
        assert_eq!(note, CursorNote::SkipsAhead);
    }

    /// The bar's total is this count, so it must match the walk loop's
    /// request count exactly or "N queued" ends above zero.
    #[test]
    fn window_count_matches_the_walk() {
        let walk = |mut c: i64, now: i64, stride: i64| {
            let mut n = 0;
            while c < now {
                n += 1;
                c = c.saturating_add(stride).max(c + 1);
            }
            n
        };
        for (c, now, stride) in [
            (0, 0, HOUR),
            (5, 0, HOUR),
            (0, HOUR, HOUR),
            (0, HOUR + 1, HOUR),
            (0, 10 * HOUR - 1, HOUR),
            (3, 1_000, 7),
            (0, 5, 0),
        ] {
            assert_eq!(
                window_count(c, now, stride),
                walk(c, now, stride),
                "{c} {now} {stride}"
            );
        }
    }

    #[test]
    fn stable_config_never_reports_skips_ahead() {
        // Without a recorded move, a start that simply sits ahead of the
        // resume cursor must not warn on every single run.
        let (_, note) = resume_cursor(Some(10 * HOUR), 50 * HOUR, HOUR, false, false);
        assert_eq!(note, CursorNote::Normal);
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
        /// Window start → the status it is refused with, or `None` for a
        /// connection that drops.
        failing: Mutex<HashMap<i64, Option<u16>>>,
        fail_all: bool,
        asked: Mutex<Vec<(i64, i64)>>,
        stop_at: Option<(usize, StopFlag)>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                deployed: day(0),
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
            while noon < end {
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
    }

    fn keys(rows: &[(String, String)]) -> Vec<&str> {
        rows.iter().map(|r| r.0.as_str()).collect()
    }

    fn window_key(start: i64, end: i64) -> String {
        format!("yolink_windows:{}", window_id_recipe(DEVICE, start, end))
    }

    const FIVE_MIN: i64 = 300_000;

    /// The walk resumes from the newest reading, so a window that failed
    /// behind it was never asked for again, and YoLink forgets history
    /// after about two months.
    #[tokio::test]
    async fn a_failed_window_behind_the_resume_point_is_fetched_again() {
        let st = Store::new().await;
        let cfg = sync(vec![device(DEVICE, "2369-04-01")], 7);
        let fake = Fake::new();
        fake.failing.lock().unwrap().insert(day(7), None);
        let s = st.run(&fake, &cfg, day(28)).await;
        assert_eq!((s.windows, s.windows_failed, s.errors), (4, 1, 0), "{s:?}");
        let failed = window_key(day(7), day(14) + FIVE_MIN);
        assert_eq!(keys(&st.problems().await), [failed.as_str()]);
        assert_eq!(
            st.count("SELECT COUNT(DISTINCT ts_ms) FROM yolink_readings")
                .await,
            21,
            "the failed week is missing"
        );
        fake.asked();

        fake.failing.lock().unwrap().clear();
        let s = st.run(&fake, &cfg, day(29)).await;
        assert_eq!((s.windows_failed, s.errors), (0, 0), "{s:?}");
        assert!(
            fake.asked().contains(&(day(7), day(14) + FIVE_MIN)),
            "the failed window is asked for again"
        );
        assert!(st.problems().await.is_empty(), "{:?}", st.problems().await);
        assert_eq!(
            st.count("SELECT COUNT(DISTINCT ts_ms) FROM yolink_readings")
                .await,
            29
        );
        assert_eq!(st.count("SELECT COUNT(*) FROM yolink_windows").await, 0);
    }

    /// A configured `start` before the device existed is refused; that
    /// window has nothing to retry and is no problem. A refusal once the
    /// device has readings is, until a retry says the window is not
    /// there at all.
    #[tokio::test]
    async fn a_refusal_before_the_first_reading_is_not_a_failure() {
        let st = Store::new().await;
        let cfg = sync(vec![device(DEVICE, "2369-04-01")], 7);
        let mut fake = Fake::new();
        fake.deployed = day(7);
        fake.failing.lock().unwrap().insert(day(0), Some(403));
        fake.failing.lock().unwrap().insert(day(14), Some(403));
        let s = st.run(&fake, &cfg, day(28)).await;
        assert_eq!((s.windows_failed, s.errors), (1, 0), "{s:?}");
        assert_eq!(
            keys(&st.problems().await),
            [window_key(day(14), day(21) + FIVE_MIN).as_str()]
        );

        fake.failing.lock().unwrap().insert(day(14), Some(404));
        let s = st.run(&fake, &cfg, day(29)).await;
        assert_eq!(s.windows_failed, 0, "{s:?}");
        assert!(st.problems().await.is_empty(), "{:?}", st.problems().await);
        assert_eq!(st.count("SELECT COUNT(*) FROM yolink_windows").await, 0);
    }

    /// A device with no reading at all whose every window is refused is
    /// most likely a wrong id; reading that as "not deployed yet" said
    /// nothing, run after run. It is a row on the device, and the row
    /// goes on the run that first gets a reading, the earlier refusals
    /// then being the time before it was deployed.
    #[tokio::test]
    async fn a_device_whose_every_window_is_refused_is_a_listing_row() {
        let st = Store::new().await;
        let cfg = sync(vec![device(DEVICE, "2369-04-01")], 7);
        let mut fake = Fake::new();
        fake.deployed = day(21);
        for d in [0, 7, 14, 21] {
            fake.failing.lock().unwrap().insert(day(d), Some(403));
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
        assert_eq!(st.count("SELECT COUNT(*) FROM yolink_windows").await, 0);

        fake.failing.lock().unwrap().remove(&day(21));
        let s = st.run(&fake, &cfg, day(28)).await;
        assert_eq!(s.errors, 0, "{s:?}");
        assert!(st.problems().await.is_empty(), "{:?}", st.problems().await);
        assert_eq!(st.count("SELECT COUNT(*) FROM yolink_windows").await, 0);
    }

    /// No run asks for a window of a device the config stopped naming,
    /// nor for one older than the history YoLink keeps, so their rows
    /// would stand for good.
    #[tokio::test]
    async fn failed_windows_no_run_will_ask_for_are_retired() {
        let st = Store::new().await;
        let fake = Fake::new();
        fake.failing.lock().unwrap().insert(day(7), None);
        let both = sync(
            vec![
                device(DEVICE, "2369-04-01"),
                device("cargo-bay-2", "2369-04-01"),
            ],
            7,
        );
        st.run(&fake, &both, day(28)).await;
        assert_eq!(st.count("SELECT COUNT(*) FROM yolink_windows").await, 2);

        let one = sync(vec![device(DEVICE, "2369-04-01")], 7);
        st.run(&fake, &one, day(29)).await;
        assert_eq!(
            keys(&st.problems().await),
            [window_key(day(7), day(14) + FIVE_MIN).as_str()],
            "the removed device's window went; the one still failing stays"
        );

        fake.failing.lock().unwrap().clear();
        fake.asked();
        st.run(&fake, &one, day(100)).await;
        assert!(
            !fake.asked().contains(&(day(7), day(14) + FIVE_MIN)),
            "a window YoLink no longer keeps is not asked for"
        );
        assert!(st.problems().await.is_empty(), "{:?}", st.problems().await);
        assert_eq!(st.count("SELECT COUNT(*) FROM yolink_windows").await, 0);
    }

    /// The retry pass stops where the forward walk would: a device whose
    /// every window fails is not asked for each of its failed windows,
    /// one failing request at a time.
    #[tokio::test]
    async fn the_retry_pass_gives_up_on_the_same_budget() {
        let st = Store::new().await;
        let cfg = sync(vec![device(DEVICE, "2369-04-01")], 1);
        let mut fake = Fake::new();
        for d in (0..64).step_by(2) {
            fake.failing.lock().unwrap().insert(day(d), None);
        }
        let s = st.run(&fake, &cfg, day(64)).await;
        assert_eq!((s.windows_failed, s.errors), (32, 0), "{s:?}");

        fake.fail_all = true;
        fake.asked();
        let s = st.run(&fake, &cfg, day(65)).await;
        assert_eq!(fake.asked().len(), 30, "{s:?}");
        let rows = st.problems().await;
        assert!(
            keys(&rows).contains(&format!("listing:{DEVICE}").as_str()),
            "{rows:?}"
        );
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
    /// row. The windows it failed lie ahead of where the next run starts,
    /// so that run's walk asks for them again and they go once it has.
    #[tokio::test]
    async fn an_abandoned_device_is_a_listing_row_and_its_windows_clear_when_walked() {
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
        assert_eq!(st.count("SELECT COUNT(*) FROM yolink_windows").await, 30);

        fake.fail_all = false;
        let s = st.run(&fake, &cfg, day(40)).await;
        assert_eq!((s.windows, s.windows_failed, s.errors), (40, 0, 0), "{s:?}");
        assert!(st.problems().await.is_empty(), "{:?}", st.problems().await);
        assert_eq!(st.count("SELECT COUNT(*) FROM yolink_windows").await, 0);
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
            st.count("SELECT last_ts_ms FROM yolink_devices").await,
            day(6) + DAY / 2,
            "the next run resumes after what landed"
        );
    }

    #[test]
    fn curls_refusal_names_its_status() {
        assert_eq!(
            http_status_of("curl: (22) The requested URL returned error: 404"),
            Some(404)
        );
        assert_eq!(http_status_of("curl: (6) Could not resolve host"), None);
    }

    #[test]
    fn a_window_id_names_its_device() {
        let id = window_id_recipe("deck#7 freezer", 1, 2);
        assert_eq!(schema_raw::device_of_window_id(&id), Some("deck#7 freezer"));
        assert_eq!(schema_raw::device_of_window_id("nonsense"), None);
    }
}
