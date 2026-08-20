//! `yolink-make-fixture <spec.json> <raw_dir>` — expand a TNG-themed
//! JSON spec into a YoLink doltlite raw store, ready for the render
//! step to read.
//!
//! ## Why a fixture *maker* rather than a checked-in store
//!
//! Every other fixture source in this tree is checked-in input that the
//! download step ingests. YoLink can't work that way: its downloader
//! fetches signed-URL CSVs by shelling out to `curl` (see
//! `download/mod.rs`), which is neither hermetic nor routed through the
//! HTTP transport that `datalib-step synthesize` records playback tapes
//! for. So the fixture pipeline seeds the raw store directly and runs
//! the source render-only — its config carries no `sync:`, which makes
//! `plan_download` contribute no processors (download-only vs.
//! render-only is structural here, not a flag).
//!
//! Checking in a `.doltlite_db` instead was the other option, and it's
//! worse: an opaque binary blob in git, coupled to the on-disk chunk
//! format, that nobody can read or edit. This binary is the same shape
//! as `signal-make-fixture` / `whatsapp-make-fixture`.
//!
//! ## Determinism
//!
//! Sample values come from a **pure formula** — a sine plus a
//! deterministic hash-derived jitter — never from an RNG or a clock, so
//! the same spec always produces the same readings. The bookkeeping
//! stamps and the `dolt_commit` date come from `--now` (falling back to
//! `$DATALIB_DAG_NOW`, which the DAG runner exports so a whole run
//! agrees — see the timestamp convention in AGENTS.md), so the fixture's
//! commit log reads in fixture time rather than in build time.
//!
//! That is as far as this binary can take it. The store is **not**
//! byte-stable across runs: `doltlite_raw::open` creates two commits of
//! its own before we get the pool — doltlite's "Initialize data
//! repository" and the shared layer's "schema: apply DDL" — and both
//! take the wall clock. Commit hashes chain, so those two move every
//! run and ours moves with them. Pinning the rest is still worth doing:
//! it removes the build clock from the readings, from the bookkeeping
//! columns, and from the one commit message a reader of the fixture
//! actually cares about.
//!
//! Spec shape: see `tests/fixtures/yolink_tng/tng.json`, which is the
//! only instance and documents each field inline.

use std::path::PathBuf;

use anyhow::{anyhow, Context, Result};
use datalib_etl::bulk::bulk_upsert_in_tx;
use datalib_etl_yolink::download::schema_raw::{YolinkDeviceRow, YolinkReadingRow};
use datalib_etl_yolink::download::{db_path_for, RawDb};
use serde::Deserialize;

#[derive(Debug, Deserialize)]
struct Spec {
    /// Unix ms of the first sample.
    start_ms: i64,
    /// Spacing between samples, in ms.
    interval_ms: i64,
    /// Samples per (device, metric).
    samples: usize,
    devices: Vec<DeviceSpec>,
}

#[derive(Debug, Deserialize)]
struct DeviceSpec {
    name: String,
    /// `temperature_humidity` | `watermeter` — mirrored into
    /// `yolink_devices.kind`, exactly as the downloader would.
    kind: String,
    metrics: Vec<MetricSpec>,
}

#[derive(Debug, Deserialize)]
struct MetricSpec {
    /// The literal `yolink_readings.metric` tag.
    metric: String,
    /// Centre of the generated wave (or, for a cumulative metric, the
    /// starting meter reading).
    base: f64,
    /// Peak deviation from `base`.
    #[serde(default)]
    swing: f64,
    /// Samples per full cycle of the wave.
    #[serde(default)]
    period_samples: f64,
    /// Peak deterministic jitter added on top of the wave.
    #[serde(default)]
    jitter: f64,
    /// Clamp generated values to at least this. Used for
    /// `water_consumption_gal`, which can be zero but never negative.
    #[serde(default)]
    floor: Option<f64>,
    /// Makes this a running total of the named sibling metric, starting
    /// at `base`. Reproduces YoLink's monotonic meter totalizer.
    #[serde(default)]
    cumulative_of: Option<String>,
}

const USAGE: &str = "usage: yolink-make-fixture <spec.json> <raw_dir> [--now <rfc3339>]";

fn main() -> Result<()> {
    let mut positional: Vec<String> = Vec::new();
    let mut now: Option<String> = None;
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--now" => now = Some(args.next().ok_or_else(|| anyhow!("--now needs a value"))?),
            other => positional.push(other.to_string()),
        }
    }
    let spec_path = positional.first().ok_or_else(|| anyhow!(USAGE))?.clone();
    let raw_dir = PathBuf::from(positional.get(1).ok_or_else(|| anyhow!(USAGE))?);

    // Flag, then the runner-exported run stamp, then the local clock.
    // The last is a courtesy for hand invocation; the fixture pipeline
    // always passes one, so the store never records build time.
    let now = now
        .or_else(|| std::env::var("DATALIB_DAG_NOW").ok())
        .unwrap_or_else(|| datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339());

    let spec: Spec = serde_json::from_str(
        &std::fs::read_to_string(&spec_path).with_context(|| format!("read spec {spec_path}"))?,
    )
    .with_context(|| format!("parse spec {spec_path}"))?;

    std::fs::create_dir_all(&raw_dir).with_context(|| format!("mkdir -p {}", raw_dir.display()))?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let written = rt.block_on(write_store(&spec, &raw_dir, &now))?;

    // One line on stdout so a genrule / driver script can see what
    // landed. The workspace bans println! generally; this binary's whole
    // output contract is that line.
    #[allow(clippy::disallowed_macros)]
    {
        println!(
            "{} devices={} readings={}",
            raw_dir.display(),
            spec.devices.len(),
            written
        );
    }
    Ok(())
}

async fn write_store(spec: &Spec, raw_dir: &std::path::Path, now: &str) -> Result<usize> {
    let db_path = db_path_for(raw_dir);
    let db = RawDb::open(&db_path)
        .await
        .with_context(|| format!("open {}", db_path.display()))?;
    let pool = db.pool();

    // The run-pinned stamp rather than `now_local()`: bookkeeping columns
    // land in the store, and a wall-clock value there would put build
    // time inside a fixture that is supposed to read in fixture time.
    let stamped_at = now;

    let device_rows: Vec<YolinkDeviceRow> = spec
        .devices
        .iter()
        .map(|d| YolinkDeviceRow {
            id: d.name.clone(),
            // A fixture device id, not a credential: the real column
            // holds half of a per-device read secret, so the value here
            // is deliberately a recognizable fake.
            family_device_id: fake_device_id(&d.name),
            kind: d.kind.clone(),
            start_ms: spec.start_ms,
        })
        .collect();

    let mut reading_rows: Vec<YolinkReadingRow> = Vec::new();
    for device in &spec.devices {
        for metric in &device.metrics {
            let values = generate(spec, device, metric)?;
            for (i, value) in values.iter().enumerate() {
                let ts_ms = spec.start_ms + (i as i64) * spec.interval_ms;
                // The downloader stores the source CSV row here so the
                // wire record survives upstream pruning. Nothing
                // downstream parses it back, so the fixture records what
                // it actually is.
                let payload = serde_json::json!({
                    "_fixture": "yolink_tng",
                    "Time": iso(ts_ms),
                    metric.metric.clone(): format!("{value:.3}"),
                })
                .to_string();
                reading_rows.push(YolinkReadingRow::new(
                    &device.name,
                    ts_ms,
                    &metric.metric,
                    *value,
                    payload,
                ));
            }
        }
    }

    let mut tx = pool.begin().await?;
    bulk_upsert_in_tx(&mut tx, &device_rows, stamped_at).await?;
    bulk_upsert_in_tx(&mut tx, &reading_rows, stamped_at).await?;
    tx.commit().await?;

    // Advance each device's cursor the way a real fetch would, so the
    // rendered page's "cursor at …" line is populated.
    let last_ts = spec.start_ms + (spec.samples as i64 - 1) * spec.interval_ms;
    sqlx::query("UPDATE yolink_devices SET last_ts_ms = ?1")
        .bind(last_ts)
        .execute(pool)
        .await?;

    // The render cursor keys off HEAD, so the store must have a commit
    // — without one `dolt_log()` is empty, the cursor is never written,
    // and every run cold-starts.
    // `--date` pins the commit's timestamp; without it `dolt_log()`
    // reports build time, which then shows up in the rendered page's
    // commit-log table. (The two commits `doltlite_raw::open` already
    // made are still wall-clock — see the module docs.)
    sqlx::query("SELECT dolt_commit('-Am', 'yolink tng fixture', '--date', ?1)")
        .bind(now)
        .execute(pool)
        .await
        .context("dolt_commit the fixture store")?;

    Ok(reading_rows.len())
}

/// Values for one (device, metric) series.
///
/// Pure function of the spec — see the module docs on determinism.
fn generate(spec: &Spec, device: &DeviceSpec, metric: &MetricSpec) -> Result<Vec<f64>> {
    if let Some(source) = &metric.cumulative_of {
        let per_sample = device
            .metrics
            .iter()
            .find(|m| &m.metric == source)
            .ok_or_else(|| {
                anyhow!(
                    "device {:?}: metric {:?} is cumulative_of {:?}, which it does not declare",
                    device.name,
                    metric.metric,
                    source
                )
            })?;
        let deltas = generate(spec, device, per_sample)?;
        let mut total = metric.base;
        return Ok(deltas
            .iter()
            .map(|d| {
                total += d;
                round3(total)
            })
            .collect());
    }

    let period = if metric.period_samples > 0.0 {
        metric.period_samples
    } else {
        1.0
    };
    Ok((0..spec.samples)
        .map(|i| {
            let phase = std::f64::consts::TAU * (i as f64) / period;
            let mut v = metric.base
                + metric.swing * phase.sin()
                + metric.jitter * jitter(&device.name, &metric.metric, i);
            if let Some(floor) = metric.floor {
                v = v.max(floor);
            }
            round3(v)
        })
        .collect())
}

/// Deterministic pseudo-jitter in `[-1, 1]`.
///
/// A 64-bit FNV-1a over `(device, metric, index)` folded into a float.
/// Chosen over `rand` because the genrule caches on output bytes: the
/// series must be identical on every machine, forever, with no seed to
/// thread around.
fn jitter(device: &str, metric: &str, i: usize) -> f64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in device
        .as_bytes()
        .iter()
        .chain(b"|")
        .chain(metric.as_bytes())
        .chain(b"|")
        .chain(&(i as u64).to_be_bytes())
    {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    // Top 24 bits → [0, 1) → [-1, 1).
    ((h >> 40) as f64 / (1u64 << 24) as f64) * 2.0 - 1.0
}

/// Three decimals, matching the precision the real CSVs carry.
fn round3(v: f64) -> f64 {
    (v * 1000.0).round() / 1000.0
}

/// Obviously-fake stand-in for the 32-hex `family_device_id`. Derived
/// from the name so it is stable, and prefixed `1701` so it reads as
/// test data in any debugger — the same convention the other TNG
/// fixtures use for UUIDs.
fn fake_device_id(name: &str) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in name.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("1701{:016x}{:012x}", h, h >> 16)
}

fn iso(ms: i64) -> String {
    datalib_time::IsoOffsetTimestamp::from_unix_millis(ms)
        .map(|t| t.to_rfc3339())
        .unwrap_or_default()
}
