//! AirVisual Pro export → doltlite. Walk the device's data folder, read
//! every history file whose content changed since the last run, and
//! upsert its samples. The current month's file grows every few
//! minutes and is re-read whole each run; the rest cost a `stat`.

pub mod parse;
pub mod schema_raw;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;
use tracing::{info, warn};

use datalib_etl::bulk::bulk_upsert_in_tx;
use datalib_etl::control::DownloadControl;
use datalib_etl::doltlite_raw as dr;
use datalib_etl::file_checkpoint;
use datalib_etl::fingerprint_cache::FingerprintCache;
use datalib_etl::fsscan::{self, ScannedFile};
use datalib_etl::progress::Progress;
use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_macros::RawStoreHandle;

use schema_raw::{full_ddl, AirvisualDeviceRow, AirvisualSampleRow, CURSOR_SCOPE, DATA_TABLES};

pub use datalib_etl::doltlite_raw::db_path_for;

const HISTORY_SUFFIX: &str = "_AirVisual_values.txt";
const LATEST_JSON: &str = "latest_config_measurements.json";

#[derive(Clone, Debug, RawStoreHandle)]
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

    pub async fn close(self) {
        self.close_all().await;
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn reset(&self) -> Result<()> {
        for table in DATA_TABLES {
            // Audited: `table` iterates a `&'static str` const array of our own
            // table names; no runtime data reaches the statement.
            sqlx::query(sqlx::AssertSqlSafe(format!("DELETE FROM {table}")))
                .execute(&self.pool)
                .await?;
        }
        file_checkpoint::clear_scope(&self.pool, CURSOR_SCOPE).await
    }
}

pub struct FetchOptions {
    /// The store this run writes into, opened and closed by the caller.
    /// A download never opens a store of its own: two live connections to
    /// one `.doltlite_db` make each other's `dolt_commit` fail. See
    /// `datalib/backend/etl/README.md`.
    pub db: RawDb,
    pub input_path: PathBuf,
    /// Configured device name; `None` reads it from the folder.
    pub device: Option<String>,
    pub cache: FingerprintCache,
    pub progress: Progress,
    pub control: DownloadControl,
}

#[derive(Debug, Default, Clone)]
pub struct FetchSummary {
    pub files: usize,
    pub files_skipped: usize,
    pub lines: usize,
    pub samples: usize,
    pub sentinels: usize,
    pub clock_unset: usize,
    pub bad_lines: usize,
    pub errors: usize,
}

/// What the folder's `latest_config_measurements.json` says about the
/// device. Every field is optional because a copied folder may lack the
/// file, and an older firmware may lack a key.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct DeviceInfo {
    pub node_name: Option<String>,
    pub serial_number: Option<String>,
    pub model: Option<String>,
    pub timezone: Option<String>,
}

pub fn read_device_info(root: &Path) -> DeviceInfo {
    let path = root.join(LATEST_JSON);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return DeviceInfo::default();
    };
    let v: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(e) => {
            warn!(event = "airvisual_latest_json_unreadable", path = %path.display(), error = %e);
            return DeviceInfo::default();
        }
    };
    let s = |v: &serde_json::Value| v.as_str().map(str::to_string);
    DeviceInfo {
        node_name: s(&v["settings"]["node_name"]),
        serial_number: s(&v["serial_number"]),
        // The Pro reports `"model": 30` as a number; keep the text form.
        model: v["status"]["model"]
            .as_i64()
            .map(|m| m.to_string())
            .or_else(|| s(&v["status"]["model"])),
        timezone: s(&v["settings"]["timezone"]),
    }
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db = opts.db;
    if opts.control.reset_and_redownload {
        db.reset().await?;
    }

    let info = read_device_info(&opts.input_path);
    let device = match opts.device.clone().or_else(|| info.node_name.clone()) {
        Some(d) => d,
        None => anyhow::bail!(
            "airvisual: no `device` configured and {} has no {LATEST_JSON} to read a name from",
            opts.input_path.display()
        ),
    };
    upsert_device(&db, &device, &info).await?;

    let scan = fsscan::scan(
        &opts.cache,
        &opts.input_path,
        &fsscan::ScanOptions::default(),
        |p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.ends_with(HISTORY_SUFFIX))
        },
    )
    .await?;

    let mut s = FetchSummary::default();
    s.errors += scan.errors.len();
    for e in &scan.errors {
        warn!(event = "airvisual_walk_error", path = %e.path.display(), error = %e.error);
    }

    let prev = file_checkpoint::load_cursor(db.pool(), CURSOR_SCOPE).await?;
    let changes = scan.changes_since(&prev);
    s.files = scan.files.len();
    s.files_skipped = changes.unchanged;
    opts.progress.set_length(Some(scan.files.len() as u64));
    opts.progress.inc(changes.unchanged as u64);

    // Sorted so the same timestamp appearing in two files (an archive
    // boundary, or a `corrupt_`/`restored_` pair) resolves the same way
    // every run: the later path wins.
    let mut todo: Vec<&ScannedFile> = changes.needs_reading().collect();
    todo.sort_by(|a, b| a.rel.cmp(&b.rel));
    for f in todo {
        opts.progress.set_message(&format!("airvisual: {}", f.rel));
        match ingest_one(&db, &device, f).await {
            Ok(stats) => {
                s.lines += stats.lines;
                s.samples += stats.samples;
                s.sentinels += stats.sentinels;
                s.clock_unset += stats.clock_unset;
                s.bad_lines += stats.bad_lines;
                info!(
                    event = "airvisual_file",
                    file = %f.rel,
                    lines = stats.lines,
                    samples = stats.samples,
                    clock_unset = stats.clock_unset,
                    bad_lines = stats.bad_lines,
                );
            }
            Err(e) => {
                s.errors += 1;
                warn!(event = "airvisual_file_failed", file = %f.rel, error = %format!("{e:#}"));
            }
        }
        opts.progress.inc(1);
    }

    sqlx::query(
        "UPDATE airvisual_devices SET last_ts_ms =
            (SELECT MAX(ts_ms) FROM airvisual_samples WHERE device_name = ?)
         WHERE id = ?",
    )
    .bind(&device)
    .bind(&device)
    .execute(db.pool())
    .await?;
    Ok(s)
}

async fn upsert_device(db: &RawDb, device: &str, info: &DeviceInfo) -> Result<()> {
    let row = AirvisualDeviceRow {
        id: device.to_string(),
        serial_number: info.serial_number.clone(),
        model: info.model.clone(),
        timezone: info.timezone.clone(),
        node_name: info.node_name.clone(),
        last_ts_ms: None,
    };
    let now = datalib_time::IsoOffsetTimestamp::now_local();
    let mut tx = db.pool().begin().await?;
    bulk_upsert_in_tx(&mut tx, &[row], &now).await?;
    tx.commit().await?;
    Ok(())
}

/// Parse one file and write its rows and its cursor stamp in one
/// transaction, so a crash between the two cannot leave a stamp for
/// rows that never landed.
async fn ingest_one(db: &RawDb, device: &str, f: &ScannedFile) -> Result<parse::ParseStats> {
    let body =
        std::fs::read_to_string(&f.path).with_context(|| format!("read {}", f.path.display()))?;
    let parsed = parse::parse(&body, &f.rel).with_context(|| format!("parse {}", f.rel))?;
    let rows: Vec<AirvisualSampleRow> = parsed
        .samples
        .into_iter()
        .map(|s| sample_row(device, s, &f.rel))
        .collect();
    let now = datalib_time::IsoOffsetTimestamp::now_local();
    let mut tx = db.pool().begin().await?;
    bulk_upsert_in_tx(&mut tx, &rows, &now).await?;
    file_checkpoint::record_file(&mut tx, CURSOR_SCOPE, f).await?;
    tx.commit().await?;
    Ok(parsed.stats)
}

fn sample_row(device: &str, s: parse::Sample, source_file: &str) -> AirvisualSampleRow {
    AirvisualSampleRow {
        id_and_payload: dr::WirePayload {
            id: schema_raw::sample_id_recipe(device, s.ts_ms),
            payload: s.payload,
        },
        device_name: device.to_string(),
        ts_ms: s.ts_ms,
        pm25_ugm3: s.pm25_ugm3,
        pm10_ugm3: s.pm10_ugm3,
        pm1_ugm3: s.pm1_ugm3,
        aqi_us: s.aqi_us,
        aqi_cn: s.aqi_cn,
        outdoor_aqi_us: s.outdoor_aqi_us,
        outdoor_aqi_cn: s.outdoor_aqi_cn,
        temperature_c: s.temperature_c,
        humidity_pct: s.humidity_pct,
        co2_ppm: s.co2_ppm,
        voc_ppb: s.voc_ppb,
        source_file: source_file.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HEADER: &str = "Date;Time;Timestamp;PM2_5(ug/m3);AQI(US);AQI(CN);PM10(ug/m3);PM1(ug/m3);Outdoor AQI(US);Outdoor AQI(CN);Temperature(C);Temperature(F);Humidity(%RH);CO2(ppm);\n";

    fn line(ts: i64, pm25: &str, co2: &str) -> String {
        format!("2026/09/01;00:00:00;{ts};{pm25};6;1;1.0;1.0;0;0;23.5;74.3;48;{co2};\n")
    }

    async fn test_cache() -> FingerprintCache {
        let d = Box::leak(Box::new(tempfile::tempdir().unwrap()));
        FingerprintCache::open(&d.path().join("fpcache.sqlite"))
            .await
            .unwrap()
    }

    struct Env {
        _dir: tempfile::TempDir,
        root: PathBuf,
        db: RawDb,
        cache: FingerprintCache,
    }

    async fn env() -> Env {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("airvisual");
        std::fs::create_dir_all(root.join("archive1")).unwrap();
        let db = RawDb::open(&dir.path().join("av.doltlite_db"))
            .await
            .unwrap();
        Env {
            root,
            db,
            cache: test_cache().await,
            _dir: dir,
        }
    }

    fn opts(e: &Env, device: Option<&str>) -> FetchOptions {
        FetchOptions {
            db: e.db.clone(),
            input_path: e.root.clone(),
            device: device.map(str::to_string),
            cache: e.cache.clone(),
            progress: Progress::default(),
            control: DownloadControl::default(),
        }
    }

    async fn count(pool: &SqlitePool, sql: &'static str) -> i64 {
        sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
    }

    #[tokio::test]
    async fn walks_archives_and_skips_unchanged_files_next_run() {
        let e = env().await;
        std::fs::write(
            e.root.join("202609_AirVisual_values.txt"),
            format!(
                "{HEADER}{}{}",
                line(1788220836, "1.0", "425"),
                line(1788221736, "2.0", "430")
            ),
        )
        .unwrap();
        std::fs::write(
            e.root.join("archive1/202501_AirVisual_values.txt"),
            format!("{HEADER}{}", line(1736185260, "3.0", "500")),
        )
        .unwrap();
        // Not a history file: never read.
        std::fs::write(e.root.join("history.txt"), "{}").unwrap();

        let s = fetch(opts(&e, Some("kitchen"))).await.unwrap();
        assert_eq!((s.files, s.files_skipped, s.lines, s.errors), (2, 0, 3, 0));
        assert_eq!(s.samples, 3);
        assert_eq!(
            count(e.db.pool(), "SELECT COUNT(*) FROM airvisual_samples").await,
            3
        );
        let co2: f64 =
            sqlx::query_scalar("SELECT co2_ppm FROM airvisual_samples WHERE ts_ms = 1788221736000")
                .fetch_one(e.db.pool())
                .await
                .unwrap();
        assert_eq!(co2, 430.0);
        let last: i64 = count(
            e.db.pool(),
            "SELECT last_ts_ms FROM airvisual_devices WHERE id = 'kitchen'",
        )
        .await;
        assert_eq!(last, 1_788_221_736_000);

        let again = fetch(opts(&e, Some("kitchen"))).await.unwrap();
        assert_eq!((again.files, again.files_skipped, again.samples), (2, 2, 0));
        e.db.close().await;
    }

    #[tokio::test]
    async fn a_grown_file_is_reread_and_upserts_without_duplicating() {
        let e = env().await;
        let f = e.root.join("202609_AirVisual_values.txt");
        std::fs::write(&f, format!("{HEADER}{}", line(1788220836, "1.0", "425"))).unwrap();
        fetch(opts(&e, Some("kitchen"))).await.unwrap();
        std::fs::write(
            &f,
            format!(
                "{HEADER}{}{}",
                line(1788220836, "1.0", "425"),
                line(1788221736, "2.0", "430")
            ),
        )
        .unwrap();
        let s = fetch(opts(&e, Some("kitchen"))).await.unwrap();
        assert_eq!(s.files_skipped, 0);
        assert_eq!(
            count(e.db.pool(), "SELECT COUNT(*) FROM airvisual_samples").await,
            2
        );
        e.db.close().await;
    }

    #[tokio::test]
    async fn device_name_comes_from_the_folder_when_not_configured() {
        let e = env().await;
        std::fs::write(
            e.root.join(LATEST_JSON),
            r#"{"serial_number":"4133wv2jb9z","settings":{"node_name":"Cucina","timezone":"Europe/Zurich"},"status":{"model":30}}"#,
        )
        .unwrap();
        std::fs::write(
            e.root.join("202609_AirVisual_values.txt"),
            format!("{HEADER}{}", line(1788220836, "1.0", "425")),
        )
        .unwrap();
        fetch(opts(&e, None)).await.unwrap();
        let row: (String, Option<String>, Option<String>, Option<String>) =
            sqlx::query_as("SELECT id, serial_number, model, timezone FROM airvisual_devices")
                .fetch_one(e.db.pool())
                .await
                .unwrap();
        assert_eq!(
            row,
            (
                "Cucina".into(),
                Some("4133wv2jb9z".into()),
                Some("30".into()),
                Some("Europe/Zurich".into())
            )
        );
        assert_eq!(
            count(
                e.db.pool(),
                "SELECT COUNT(*) FROM airvisual_samples WHERE device_name = 'Cucina'"
            )
            .await,
            1
        );
        e.db.close().await;
    }

    #[tokio::test]
    async fn no_name_anywhere_is_an_error() {
        let e = env().await;
        std::fs::write(
            e.root.join("202609_AirVisual_values.txt"),
            format!("{HEADER}{}", line(1788220836, "1.0", "425")),
        )
        .unwrap();
        let err = fetch(opts(&e, None)).await.unwrap_err();
        assert!(format!("{err}").contains("no `device` configured"));
        e.db.close().await;
    }

    #[tokio::test]
    async fn reset_drops_rows_and_the_cursor() {
        let e = env().await;
        std::fs::write(
            e.root.join("202609_AirVisual_values.txt"),
            format!("{HEADER}{}", line(1788220836, "1.0", "425")),
        )
        .unwrap();
        fetch(opts(&e, Some("kitchen"))).await.unwrap();
        let mut o = opts(&e, Some("kitchen"));
        o.control.reset_and_redownload = true;
        let s = fetch(o).await.unwrap();
        assert_eq!(
            s.files_skipped, 0,
            "the cursor was cleared, so the file re-read"
        );
        assert_eq!(
            count(e.db.pool(), "SELECT COUNT(*) FROM airvisual_samples").await,
            1
        );
        e.db.close().await;
    }
}
