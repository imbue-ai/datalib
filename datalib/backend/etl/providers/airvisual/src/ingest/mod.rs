//! AirVisual Pro export → doltlite. For each configured device, walk
//! its data folder, read every history file whose content changed since
//! the last run, and upsert its samples. The current month's file grows
//! every few minutes and is re-read whole each run; the rest cost a
//! `stat`.

pub mod parse;
pub mod schema_raw;

use std::path::Path;

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

use datalib_etl_airvisual_config::AirvisualDevice;

use schema_raw::{
    cursor_scope, full_ddl, AirvisualDeviceRow, AirvisualSampleRow, AirvisualUnplacedSampleRow,
    CURSOR_SCOPE_PREFIX, DATA_TABLES,
};

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
        file_checkpoint::clear_scope_prefix(&self.pool, CURSOR_SCOPE_PREFIX).await
    }
}

pub struct FetchOptions {
    /// The store this run writes into, opened and closed by the caller.
    /// A download never opens a store of its own: two live connections to
    /// one `.doltlite_db` make each other's `dolt_commit` fail. See
    /// `datalib/backend/etl/README.md`.
    pub db: RawDb,
    pub devices: Vec<AirvisualDevice>,
    pub cache: FingerprintCache,
    pub progress: Progress,
    pub control: DownloadControl,
}

#[derive(Debug, Default, Clone)]
pub struct FetchSummary {
    pub devices: usize,
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
    pub mac_address: Option<String>,
    pub app_version: Option<String>,
    pub system_version: Option<String>,
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
        mac_address: s(&v["status"]["mac_address"]),
        app_version: s(&v["status"]["app_version"]),
        system_version: s(&v["status"]["system_version"]),
        timezone: s(&v["settings"]["timezone"]),
    }
}

/// Who this device is: its serial from the config or the folder, and
/// its name from the config, the folder, or the serial.
pub struct Identity {
    pub id: String,
    pub name: String,
}

pub fn identify(dev: &AirvisualDevice, info: &DeviceInfo) -> Result<Identity> {
    let id = match dev.serial.clone().or_else(|| info.serial_number.clone()) {
        Some(id) => id,
        None => anyhow::bail!(
            "airvisual: no `serial` configured for {} and it has no {LATEST_JSON} to read one from",
            dev.path.display()
        ),
    };
    let name = dev
        .name
        .clone()
        .or_else(|| info.node_name.clone())
        .unwrap_or_else(|| id.clone());
    Ok(Identity { id, name })
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db = opts.db;
    if opts.control.reset_and_redownload {
        db.reset().await?;
    }
    let mut s = FetchSummary {
        devices: opts.devices.len(),
        ..Default::default()
    };
    for dev in &opts.devices {
        if let Err(e) = fetch_device(&db, dev, &opts.cache, &opts.progress, &mut s).await {
            s.errors += 1;
            warn!(event = "airvisual_device_failed", path = %dev.path.display(), error = %format!("{e:#}"));
        }
    }
    Ok(s)
}

async fn fetch_device(
    db: &RawDb,
    dev: &AirvisualDevice,
    cache: &FingerprintCache,
    progress: &Progress,
    s: &mut FetchSummary,
) -> Result<()> {
    let root = dev.path();
    let info = read_device_info(&root);
    let who = identify(dev, &info)?;
    upsert_device(db, &who, &info).await?;
    let scope = cursor_scope(&who.id);

    let scan = fsscan::scan(cache, &root, &fsscan::ScanOptions::default(), |p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(HISTORY_SUFFIX))
    })
    .await?;
    s.errors += scan.errors.len();
    for e in &scan.errors {
        warn!(event = "airvisual_walk_error", path = %e.path.display(), error = %e.error);
    }

    let prev = file_checkpoint::load_cursor(db.pool(), &scope).await?;
    let changes = scan.changes_since(&prev);
    s.files += scan.files.len();
    s.files_skipped += changes.unchanged;
    progress.set_message(&format!("airvisual: {}", who.name));

    // Sorted so the same timestamp appearing in two files (an archive
    // boundary, or a `corrupt_`/`restored_` pair) resolves the same way
    // every run: the later path wins.
    let mut todo: Vec<&ScannedFile> = changes.needs_reading().collect();
    todo.sort_by(|a, b| a.rel.cmp(&b.rel));
    for f in todo {
        match ingest_one(db, &who.id, &scope, f).await {
            Ok(stats) => {
                s.lines += stats.lines;
                s.samples += stats.samples;
                s.sentinels += stats.sentinels;
                s.clock_unset += stats.clock_unset;
                s.bad_lines += stats.bad_lines;
                info!(
                    event = "airvisual_file",
                    device = %who.id,
                    file = %f.rel,
                    lines = stats.lines,
                    samples = stats.samples,
                    clock_unset = stats.clock_unset,
                    bad_lines = stats.bad_lines,
                );
            }
            Err(e) => {
                s.errors += 1;
                warn!(event = "airvisual_file_failed", device = %who.id, file = %f.rel, error = %format!("{e:#}"));
            }
        }
    }
    progress.inc(1);

    sqlx::query(
        "UPDATE airvisual_devices SET last_ts_ms =
            (SELECT MAX(ts_ms) FROM airvisual_samples WHERE device_id = ?)
         WHERE id = ?",
    )
    .bind(&who.id)
    .bind(&who.id)
    .execute(db.pool())
    .await?;
    Ok(())
}

/// Write the device row only when it would change: an upsert stamps
/// the bookkeeping sidecar, and a stamp on an unchanged run is a commit
/// on an unchanged store, which makes the render re-run for nothing.
async fn upsert_device(db: &RawDb, who: &Identity, info: &DeviceInfo) -> Result<()> {
    let row = AirvisualDeviceRow {
        id: who.id.clone(),
        name: who.name.clone(),
        model: info.model.clone(),
        mac_address: info.mac_address.clone(),
        app_version: info.app_version.clone(),
        system_version: info.system_version.clone(),
        timezone: info.timezone.clone(),
        last_ts_ms: None,
    };
    type Stored = (
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    );
    let stored: Option<Stored> = sqlx::query_as(
        "SELECT name, model, mac_address, app_version, system_version, timezone \
           FROM airvisual_devices WHERE id = ?",
    )
    .bind(&row.id)
    .fetch_optional(db.pool())
    .await?;
    let same = stored
        .as_ref()
        .is_some_and(|(name, model, mac, app, sys, tz)| {
            *name == row.name
                && *model == row.model
                && *mac == row.mac_address
                && *app == row.app_version
                && *sys == row.system_version
                && *tz == row.timezone
        });
    if same {
        return Ok(());
    }
    let now = datalib_time::IsoOffsetTimestamp::now_local();
    let mut tx = db.pool().begin().await?;
    bulk_upsert_in_tx(&mut tx, &[row], &now).await?;
    tx.commit().await?;
    Ok(())
}

/// Parse one file and write its rows and its cursor stamp in one
/// transaction, so a crash between the two cannot leave a stamp for
/// rows that never landed.
async fn ingest_one(
    db: &RawDb,
    device: &str,
    scope: &str,
    f: &ScannedFile,
) -> Result<parse::ParseStats> {
    let body =
        std::fs::read_to_string(&f.path).with_context(|| format!("read {}", f.path.display()))?;
    let parsed = parse::parse(&body, &f.rel).with_context(|| format!("parse {}", f.rel))?;
    let rows: Vec<AirvisualSampleRow> = parsed
        .samples
        .into_iter()
        .map(|s| sample_row(device, s, &f.rel))
        .collect();
    let unplaced: Vec<AirvisualUnplacedSampleRow> = parsed
        .unplaced
        .into_iter()
        .map(|u| AirvisualUnplacedSampleRow {
            id_and_payload: dr::WirePayload {
                id: schema_raw::unplaced_id_recipe(device, &f.rel, u.line_no),
                payload: u.payload,
            },
            device_id: device.to_string(),
            source_file: f.rel.clone(),
            line_no: u.line_no,
            device_ts_s: u.device_ts_s,
        })
        .collect();
    let now = datalib_time::IsoOffsetTimestamp::now_local();
    let mut tx = db.pool().begin().await?;
    bulk_upsert_in_tx(&mut tx, &rows, &now).await?;
    bulk_upsert_in_tx(&mut tx, &unplaced, &now).await?;
    file_checkpoint::record_file(&mut tx, scope, f).await?;
    tx.commit().await?;
    Ok(parsed.stats)
}

fn sample_row(device: &str, s: parse::Sample, source_file: &str) -> AirvisualSampleRow {
    AirvisualSampleRow {
        id_and_payload: dr::WirePayload {
            id: schema_raw::sample_id_recipe(device, s.ts_ms),
            payload: s.payload,
        },
        device_id: device.to_string(),
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
    use std::path::PathBuf;

    const HEADER: &str = "Date;Time;Timestamp;PM2_5(ug/m3);AQI(US);AQI(CN);PM10(ug/m3);PM1(ug/m3);Outdoor AQI(US);Outdoor AQI(CN);Temperature(C);Temperature(F);Humidity(%RH);CO2(ppm);\n";

    fn line(ts: i64, pm25: &str, co2: &str) -> String {
        format!("2026/09/01;00:00:00;{ts};{pm25};6;1;1.0;1.0;0;0;23.5;74.3;48;{co2};\n")
    }

    fn latest_json(serial: &str, name: &str) -> String {
        format!(
            r#"{{"serial_number":"{serial}","settings":{{"node_name":"{name}","timezone":"Europe/Zurich"}},"status":{{"model":30,"mac_address":"7c25da8d352a","app_version":"1.1937","system_version":"KBG66F85"}}}}"#
        )
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

    fn device(path: &Path, serial: Option<&str>, name: Option<&str>) -> AirvisualDevice {
        AirvisualDevice {
            path: path.to_path_buf(),
            serial: serial.map(str::to_string),
            name: name.map(str::to_string),
        }
    }

    fn opts(e: &Env, devices: Vec<AirvisualDevice>) -> FetchOptions {
        FetchOptions {
            db: e.db.clone(),
            devices,
            cache: e.cache.clone(),
            progress: Progress::default(),
            control: DownloadControl::default(),
        }
    }

    fn kitchen(e: &Env) -> Vec<AirvisualDevice> {
        vec![device(&e.root, Some("KITCHEN01"), Some("kitchen"))]
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

        let s = fetch(opts(&e, kitchen(&e))).await.unwrap();
        assert_eq!(
            (s.devices, s.files, s.files_skipped, s.lines, s.errors),
            (1, 2, 0, 3, 0)
        );
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
            "SELECT last_ts_ms FROM airvisual_devices WHERE id = 'KITCHEN01'",
        )
        .await;
        assert_eq!(last, 1_788_221_736_000);

        let again = fetch(opts(&e, kitchen(&e))).await.unwrap();
        assert_eq!((again.files, again.files_skipped, again.samples), (2, 2, 0));
        e.db.close().await;
    }

    /// The fixture pipeline asserts an unchanged run reads nothing
    /// downstream, which holds only if an unchanged run writes nothing.
    #[tokio::test]
    async fn an_unchanged_run_leaves_the_working_set_clean() {
        let e = env().await;
        std::fs::write(
            e.root.join(LATEST_JSON),
            latest_json("KITCHEN01", "kitchen"),
        )
        .unwrap();
        std::fs::write(
            e.root.join("202609_AirVisual_values.txt"),
            format!("{HEADER}{}", line(1788220836, "1.0", "425")),
        )
        .unwrap();
        fetch(opts(&e, vec![device(&e.root, None, None)]))
            .await
            .unwrap();
        sqlx::query("SELECT dolt_commit('-Am', 'first')")
            .execute(e.db.pool())
            .await
            .unwrap();
        fetch(opts(&e, vec![device(&e.root, None, None)]))
            .await
            .unwrap();
        let dirty: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dolt_status")
            .fetch_one(e.db.pool())
            .await
            .unwrap();
        assert_eq!(dirty, 0, "a second identical run must not touch the store");
        e.db.close().await;
    }

    #[tokio::test]
    async fn a_grown_file_is_reread_and_upserts_without_duplicating() {
        let e = env().await;
        let f = e.root.join("202609_AirVisual_values.txt");
        std::fs::write(&f, format!("{HEADER}{}", line(1788220836, "1.0", "425"))).unwrap();
        fetch(opts(&e, kitchen(&e))).await.unwrap();
        std::fs::write(
            &f,
            format!(
                "{HEADER}{}{}",
                line(1788220836, "1.0", "425"),
                line(1788221736, "2.0", "430")
            ),
        )
        .unwrap();
        let s = fetch(opts(&e, kitchen(&e))).await.unwrap();
        assert_eq!(s.files_skipped, 0);
        assert_eq!(
            count(e.db.pool(), "SELECT COUNT(*) FROM airvisual_samples").await,
            2
        );
        e.db.close().await;
    }

    #[tokio::test]
    async fn identity_comes_from_the_folder_when_not_configured() {
        let e = env().await;
        std::fs::write(
            e.root.join(LATEST_JSON),
            latest_json("4133wv2jb9z", "Cucina"),
        )
        .unwrap();
        std::fs::write(
            e.root.join("202609_AirVisual_values.txt"),
            format!("{HEADER}{}", line(1788220836, "1.0", "425")),
        )
        .unwrap();
        fetch(opts(&e, vec![device(&e.root, None, None)]))
            .await
            .unwrap();
        let row: (
            String,
            String,
            Option<String>,
            Option<String>,
            Option<String>,
        ) = sqlx::query_as("SELECT id, name, model, mac_address, timezone FROM airvisual_devices")
            .fetch_one(e.db.pool())
            .await
            .unwrap();
        assert_eq!(
            row,
            (
                "4133wv2jb9z".into(),
                "Cucina".into(),
                Some("30".into()),
                Some("7c25da8d352a".into()),
                Some("Europe/Zurich".into())
            )
        );
        assert_eq!(
            count(
                e.db.pool(),
                "SELECT COUNT(*) FROM airvisual_samples WHERE device_id = '4133wv2jb9z'"
            )
            .await,
            1
        );
        e.db.close().await;
    }

    #[tokio::test]
    async fn the_config_overrides_the_folder_and_the_serial_stands_in_for_a_name() {
        let e = env().await;
        std::fs::write(
            e.root.join(LATEST_JSON),
            latest_json("4133wv2jb9z", "Cucina"),
        )
        .unwrap();
        std::fs::write(
            e.root.join("202609_AirVisual_values.txt"),
            format!("{HEADER}{}", line(1788220836, "1.0", "425")),
        )
        .unwrap();
        fetch(opts(&e, vec![device(&e.root, Some("OVERRIDE"), None)]))
            .await
            .unwrap();
        let row: (String, String) = sqlx::query_as("SELECT id, name FROM airvisual_devices")
            .fetch_one(e.db.pool())
            .await
            .unwrap();
        assert_eq!(row, ("OVERRIDE".into(), "Cucina".into()));
        e.db.close().await;
    }

    #[tokio::test]
    async fn two_devices_with_identical_file_names_keep_separate_cursors() {
        let e = env().await;
        let b = e.root.parent().unwrap().join("second");
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(
            e.root.join("202609_AirVisual_values.txt"),
            format!("{HEADER}{}", line(1788220836, "1.0", "425")),
        )
        .unwrap();
        std::fs::write(
            b.join("202609_AirVisual_values.txt"),
            format!(
                "{HEADER}{}{}",
                line(1788220836, "5.0", "900"),
                line(1788220846, "5.0", "901")
            ),
        )
        .unwrap();
        let devices = || {
            vec![
                device(&e.root, Some("A"), Some("kitchen")),
                device(&b, Some("B"), Some("bedroom")),
            ]
        };
        let s = fetch(opts(&e, devices())).await.unwrap();
        assert_eq!((s.devices, s.files, s.samples, s.errors), (2, 2, 3, 0));
        assert_eq!(
            count(
                e.db.pool(),
                "SELECT COUNT(*) FROM airvisual_samples WHERE device_id = 'B'"
            )
            .await,
            2
        );
        assert_eq!(
            count(e.db.pool(), "SELECT COUNT(*) FROM ingested_files").await,
            2,
            "one cursor row per (device, file)"
        );
        let again = fetch(opts(&e, devices())).await.unwrap();
        assert_eq!((again.files, again.files_skipped), (2, 2));
        e.db.close().await;
    }

    #[tokio::test]
    async fn pre_clock_lines_land_in_the_unplaced_table_once() {
        let e = env().await;
        std::fs::write(
            e.root.join("archive1/197001_AirVisual_values.txt"),
            format!(
                "{HEADER}{}{}",
                line(254, "0.0", "683"),
                line(338027, "", "350")
            ),
        )
        .unwrap();
        for _ in 0..2 {
            let s = fetch(opts(&e, kitchen(&e))).await.unwrap();
            assert_eq!(s.samples, 0, "nothing placeable in time");
        }
        assert_eq!(
            count(e.db.pool(), "SELECT COUNT(*) FROM airvisual_samples").await,
            0
        );
        assert_eq!(
            count(
                e.db.pool(),
                "SELECT COUNT(*) FROM airvisual_unplaced_samples"
            )
            .await,
            2
        );
        let id: String = sqlx::query_scalar(
            "SELECT id FROM airvisual_unplaced_samples ORDER BY line_no LIMIT 1",
        )
        .fetch_one(e.db.pool())
        .await
        .unwrap();
        assert_eq!(id, "KITCHEN01#archive1/197001_AirVisual_values.txt#2");
        e.db.close().await;
    }

    #[tokio::test]
    async fn a_device_with_no_serial_anywhere_fails_alone() {
        let e = env().await;
        std::fs::write(
            e.root.join("202609_AirVisual_values.txt"),
            format!("{HEADER}{}", line(1788220836, "1.0", "425")),
        )
        .unwrap();
        let s = fetch(opts(&e, vec![device(&e.root, None, None)]))
            .await
            .unwrap();
        assert_eq!((s.devices, s.errors, s.samples), (1, 1, 0));
        e.db.close().await;
    }

    #[tokio::test]
    async fn reset_drops_rows_and_the_cursors() {
        let e = env().await;
        std::fs::write(
            e.root.join("202609_AirVisual_values.txt"),
            format!("{HEADER}{}", line(1788220836, "1.0", "425")),
        )
        .unwrap();
        fetch(opts(&e, kitchen(&e))).await.unwrap();
        let mut o = opts(&e, kitchen(&e));
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
