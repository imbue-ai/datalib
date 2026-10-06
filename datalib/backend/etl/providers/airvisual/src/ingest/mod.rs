//! AirVisual Pro export → doltlite. For each configured device, walk
//! its data folder, read every history file whose content changed since
//! the last run, and upsert its samples — all of one device's files in
//! one transaction, since a full re-read is a minute and every SQL
//! commit rewrites the store's tree. The current month's file grows
//! every few minutes and is re-read whole each run; the rest cost a
//! `stat`.

pub mod parse;
pub mod schema_raw;

use std::collections::HashSet;
use std::path::Path;

use anyhow::Result;
use sqlx::{Sqlite, Transaction};
use tracing::info;

use datalib_etl::bulk::bulk_upsert_entity_in_tx;
use datalib_etl::control::DownloadControl;
use datalib_etl::doltlite_raw as dr;
use datalib_etl::file_checkpoint;
use datalib_etl::fingerprint_cache::FingerprintCache;
use datalib_etl::fsscan::{self, ScannedFile};
use datalib_etl::progress::Progress;
use datalib_etl::run_problems::{self, RunProblems};

use datalib_etl_airvisual_config::AirvisualDevice;
use datalib_problems::{Outcome, Problem, Reason};

use schema_raw::{
    cursor_scope, full_ddl, AirvisualDeviceRow, AirvisualSampleRow, AirvisualUnplacedSampleRow,
};

pub use datalib_etl::doltlite_raw::db_path_for;

const HISTORY_SUFFIX: &str = "_AirVisual_values.txt";
const LATEST_JSON: &str = "latest_config_measurements.json";

datalib_etl::raw_db!(pub RawDb: EntityStore, full_ddl());

pub struct FetchOptions {
    /// The store this run writes into, opened and closed by the caller.
    /// A download never opens a store of its own: one writer per file
    /// (`datalib/backend/etl/README.md` § "One writer per file, by
    /// construction").
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

/// What the folder says about its device. A folder without the file
/// says nothing, which is not an error; a file that will not read or
/// parse is, since what it would have said is unknown.
pub fn read_device_info(root: &Path) -> std::result::Result<DeviceInfo, String> {
    let path = root.join(LATEST_JSON);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(DeviceInfo::default()),
        Err(e) => return Err(format!("read {LATEST_JSON}: {e}")),
    };
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("parse {LATEST_JSON}: {e}"))?;
    let s = |v: &serde_json::Value| v.as_str().map(str::to_string);
    Ok(DeviceInfo {
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
    })
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

/// The raw table a file's `record:` problem row names; the id is
/// `<device>/<path under its folder>`.
pub const FILES_TABLE: &str = "airvisual_files";

/// Which devices a run reached, so its end can say which file rows it
/// has a verdict on. Every run re-reads every file it could not stamp,
/// so a device it read has exactly the file rows this run found.
struct Reached {
    found: RunProblems,
    /// Serials whose folder was walked this run.
    read: HashSet<String>,
    /// Serials whose folder could not be walked this run.
    unread: HashSet<String>,
    /// A device whose serial is unknown failed, so any serial could be it.
    unidentified: bool,
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let (pool, stop) = (opts.db.pool().clone(), opts.control.stop.clone());
    run_problems::collecting(&pool, &stop, |found| read_devices(opts, found)).await
}

async fn read_devices(opts: FetchOptions, found: RunProblems) -> Result<FetchSummary> {
    let db = opts.db;
    let mut s = FetchSummary {
        devices: opts.devices.len(),
        ..Default::default()
    };
    let mut reached = Reached {
        found: found.clone(),
        read: HashSet::new(),
        unread: HashSet::new(),
        unidentified: false,
    };
    for dev in &opts.devices {
        if opts.control.stop.requested() {
            break;
        }
        fetch_device(&db, dev, &opts.cache, &opts.progress, &mut s, &mut reached).await?;
    }
    // A stop leaves devices unvisited, and any serial could be one of them.
    let any_unread = reached.unidentified || opts.control.stop.requested();
    let Reached { read, unread, .. } = reached;
    // A serial neither read nor failing is a device the config no longer
    // names, and its rows go.
    found.records_tried_all_but(FILES_TABLE, move |id| {
        let serial = id.split('/').next().unwrap_or(id);
        !read.contains(serial) && (unread.contains(serial) || any_unread)
    });
    Ok(s)
}

/// One device's folder. A folder that cannot be read costs that device
/// and leaves a `listing:device <name>` row; only the store failing is
/// an `Err`.
async fn fetch_device(
    db: &RawDb,
    dev: &AirvisualDevice,
    cache: &FingerprintCache,
    progress: &Progress,
    s: &mut FetchSummary,
    reached: &mut Reached,
) -> Result<()> {
    let root = dev.path();
    let label = format!(
        "device {}",
        dev.name
            .clone()
            .unwrap_or_else(|| dev.path.display().to_string())
    );
    let found = reached.found.clone();
    let device_failed = |s: &mut FetchSummary, detail: String| {
        s.errors += 1;
        found.listing(&label, detail);
    };
    let started = std::time::Instant::now();
    let (info, info_error) = match read_device_info(&root) {
        Ok(info) => (info, None),
        Err(e) => (DeviceInfo::default(), Some(e)),
    };
    let who = match identify(dev, &info) {
        Ok(who) => who,
        Err(e) => {
            let detail = match &info_error {
                Some(why) => format!("{e:#}; {why}"),
                None => format!("{e:#}"),
            };
            device_failed(s, detail);
            reached.unidentified = true;
            return Ok(());
        }
    };
    let scope = cursor_scope(&who.id);
    let identified_ms = started.elapsed().as_millis();

    let scan = match fsscan::scan(cache, &root, &fsscan::ScanOptions::default(), |p| {
        p.file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.ends_with(HISTORY_SUFFIX))
    })
    .await
    {
        Ok(scan) => scan,
        Err(e) => {
            device_failed(s, format!("{e:#}"));
            reached.unread.insert(who.id.clone());
            return Ok(());
        }
    };
    reached.read.insert(who.id.clone());
    let file_failed = |rel: &str, detail: String| {
        found.record_failed(FILES_TABLE, &format!("{}/{rel}", who.id), detail)
    };
    if let Some(why) = &info_error {
        s.errors += 1;
        file_failed(LATEST_JSON, why.clone());
    }
    info!(
        event = "airvisual_scan",
        device = %who.id,
        files = scan.files.len(),
        hashed = scan.stats.hashed,
        identified_ms,
        scan_ms = started.elapsed().as_millis() - identified_ms,
        "scanned the export tree"
    );
    s.errors += scan.errors.len();
    found.extend(scan.walk_problems_as(&format!("files {}", who.id)));

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
    let mut tx = db.pool().begin().await?;
    upsert_device(&mut tx, &who, &info, info_error.is_none()).await?;
    for f in todo {
        let file_started = std::time::Instant::now();
        let read = match read_one(f) {
            Ok(read) => read,
            Err(Unread::Read(why)) => {
                // Not stamped, so the next run reads it again.
                s.errors += 1;
                file_failed(&f.rel, why);
                continue;
            }
            Err(Unread::Parse(why)) => {
                // The same bytes would not parse next run either: stamped
                // with its problem, it is read again once it changes.
                s.errors += 1;
                let problem = Problem::record(Reason::Undeserializable, &why);
                file_checkpoint::record_file_with_problem(
                    &mut tx,
                    &scope,
                    f,
                    Some((Outcome::Dropped, problem)),
                )
                .await?;
                continue;
            }
        };
        let (stats, timing) = write_one(&mut tx, &who.id, &scope, f, read).await?;
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
            read_ms = timing.read_ms,
            parse_ms = timing.parse_ms,
            upsert_ms = timing.upsert_ms,
            total_ms = file_started.elapsed().as_millis(),
            "ingested one measurement file"
        );
    }
    sqlx::query(
        "UPDATE airvisual_devices SET last_ts_ms =
            (SELECT MAX(ts_ms) FROM airvisual_samples WHERE device_id = ?)
         WHERE id = ?",
    )
    .bind(&who.id)
    .bind(&who.id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    progress.inc(1);
    Ok(())
}

/// Write the device row only when it would change, so a run that finds
/// nothing new leaves the store as it was and the render has nothing
/// to redo. When the folder's description could not be read
/// (`info_known` false) a stored row is left as it is rather than
/// blanked.
async fn upsert_device(
    tx: &mut Transaction<'_, Sqlite>,
    who: &Identity,
    info: &DeviceInfo,
    info_known: bool,
) -> Result<()> {
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
    .fetch_optional(&mut **tx)
    .await?;
    if !info_known && stored.is_some() {
        return Ok(());
    }
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
    bulk_upsert_entity_in_tx(tx, &[row]).await
}

/// Where one file's time went, for the `airvisual_file` event.
struct FileTiming {
    read_ms: u128,
    parse_ms: u128,
    upsert_ms: u128,
}

struct ReadFile {
    parsed: parse::Parsed,
    read_ms: u128,
    parse_ms: u128,
}

/// Why a file was not read.
enum Unread {
    Read(String),
    Parse(String),
}

/// Read and parse one file. An `Err` costs that file and nothing else.
fn read_one(f: &ScannedFile) -> std::result::Result<ReadFile, Unread> {
    let t = std::time::Instant::now();
    let body = std::fs::read_to_string(&f.path)
        .map_err(|e| Unread::Read(format!("read {}: {e}", f.path.display())))?;
    let read_ms = t.elapsed().as_millis();
    let parsed = parse::parse(&body, &f.rel)
        .map_err(|e| Unread::Parse(format!("parse {}: {e:#}", f.rel)))?;
    Ok(ReadFile {
        parsed,
        read_ms,
        parse_ms: t.elapsed().as_millis() - read_ms,
    })
}

/// Write one parsed file's rows and its cursor stamp into the device's
/// transaction.
async fn write_one(
    tx: &mut Transaction<'_, Sqlite>,
    device: &str,
    scope: &str,
    f: &ScannedFile,
    read: ReadFile,
) -> Result<(parse::ParseStats, FileTiming)> {
    let t = std::time::Instant::now();
    let ReadFile {
        parsed,
        read_ms,
        parse_ms,
    } = read;
    let problem = bad_lines_problem(&parsed);
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
    schema_raw::upsert_samples(tx, &rows).await?;
    bulk_upsert_entity_in_tx(tx, &unplaced).await?;
    file_checkpoint::record_file_with_problem(tx, scope, f, problem).await?;
    let upsert_ms = t.elapsed().as_millis();
    Ok((
        parsed.stats,
        FileTiming {
            read_ms,
            parse_ms,
            upsert_ms,
        },
    ))
}

/// The file's one problem row, when some of its lines could not be
/// used: it stands until the file changes and is read again.
fn bad_lines_problem(parsed: &parse::Parsed) -> Option<(Outcome, Problem)> {
    let first = parsed.first_bad.as_deref()?;
    let (bad, dropped) = (parsed.stats.bad_lines, parsed.lines_dropped);
    let outcome = if dropped > 0 {
        Outcome::Dropped
    } else {
        Outcome::Nulled
    };
    Some((
        outcome,
        Problem::record(
            Reason::Undeserializable,
            &format!(
                "{bad} lines could not be read whole ({dropped} not stored at all); first: {first}"
            ),
        ),
    ))
}

fn sample_row(device: &str, sample: parse::Sample, source_file: &str) -> AirvisualSampleRow {
    AirvisualSampleRow {
        device_id: device.to_string(),
        sample,
        source_file: source_file.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePool;
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

    /// Every `problems` row, `(scope_key, sample)`.
    async fn problems(pool: &SqlitePool) -> Vec<(String, String)> {
        sqlx::query_as("SELECT scope_key, sample FROM problems ORDER BY scope_key")
            .fetch_all(pool)
            .await
            .unwrap()
    }

    fn keys(rows: &[(String, String)]) -> Vec<&str> {
        rows.iter().map(|r| r.0.as_str()).collect()
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
        dr::commit_run(e.db.pool(), "first").await.unwrap();
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
        let rows = problems(e.db.pool()).await;
        assert_eq!(
            keys(&rows),
            [format!("listing:device {}", e.root.display())],
            "the failed device is a row, not only a log line"
        );
        assert!(rows[0].1.contains("no `serial`"), "{rows:?}");

        std::fs::write(
            e.root.join(LATEST_JSON),
            latest_json("KITCHEN01", "kitchen"),
        )
        .unwrap();
        let s = fetch(opts(&e, vec![device(&e.root, None, None)]))
            .await
            .unwrap();
        assert_eq!((s.errors, s.samples), (0, 1));
        assert!(problems(e.db.pool()).await.is_empty());
        e.db.close().await;
    }

    /// A file that will not parse would not parse next run either: it is
    /// stamped with its problem, not read again until it changes, and the
    /// row goes when it does.
    #[tokio::test]
    async fn a_file_that_will_not_parse_is_a_problem_until_it_changes() {
        let e = env().await;
        let f = e.root.join("archive1/202501_AirVisual_values.txt");
        std::fs::write(&f, "Stardate;Warp factor;\n47634.4;9.2;\n").unwrap();
        std::fs::write(
            e.root.join("202609_AirVisual_values.txt"),
            format!("{HEADER}{}", line(1788220836, "1.0", "425")),
        )
        .unwrap();
        let s = fetch(opts(&e, kitchen(&e))).await.unwrap();
        assert_eq!((s.errors, s.samples), (1, 1), "the other file still landed");
        let rows = problems(e.db.pool()).await;
        let key = "file:airvisual/export/KITCHEN01:archive1/202501_AirVisual_values.txt";
        assert_eq!(keys(&rows), [key]);
        assert!(rows[0].1.contains("no Timestamp column"), "{rows:?}");

        let s = fetch(opts(&e, kitchen(&e))).await.unwrap();
        assert_eq!(
            (s.errors, s.files_skipped),
            (0, 2),
            "an unchanged file is not read again"
        );
        assert_eq!(keys(&problems(e.db.pool()).await), [key]);

        std::fs::write(&f, format!("{HEADER}{}", line(1736185260, "3.0", "500"))).unwrap();
        let s = fetch(opts(&e, kitchen(&e))).await.unwrap();
        assert_eq!((s.errors, s.samples), (0, 1));
        assert!(problems(e.db.pool()).await.is_empty());
        e.db.close().await;
    }

    /// A folder the walk cannot enter is named, and the rest is read.
    #[tokio::test]
    async fn an_unreadable_folder_is_a_problem_until_it_reads() {
        use std::os::unix::fs::PermissionsExt;
        let e = env().await;
        std::fs::write(
            e.root.join("archive1/202501_AirVisual_values.txt"),
            format!("{HEADER}{}", line(1736185260, "3.0", "500")),
        )
        .unwrap();
        std::fs::write(
            e.root.join("202609_AirVisual_values.txt"),
            format!("{HEADER}{}", line(1788220836, "1.0", "425")),
        )
        .unwrap();
        let archive = e.root.join("archive1");
        std::fs::set_permissions(&archive, std::fs::Permissions::from_mode(0o000)).unwrap();
        // Root reads a mode-000 folder (CI's container runs as root), so
        // there is nothing to test there.
        if std::fs::read_dir(&archive).is_ok() {
            std::fs::set_permissions(&archive, std::fs::Permissions::from_mode(0o755)).unwrap();
            e.db.close().await;
            return;
        }
        let s = fetch(opts(&e, kitchen(&e))).await;
        std::fs::set_permissions(&archive, std::fs::Permissions::from_mode(0o755)).unwrap();
        let s = s.unwrap();
        assert_eq!((s.errors, s.samples), (1, 1), "{s:?}");
        let rows = problems(e.db.pool()).await;
        assert_eq!(keys(&rows), ["listing:files KITCHEN01"]);
        assert!(rows[0].1.starts_with(".: "), "{rows:?}");

        let s = fetch(opts(&e, kitchen(&e))).await.unwrap();
        assert_eq!((s.errors, s.samples), (0, 1));
        assert!(problems(e.db.pool()).await.is_empty());
        e.db.close().await;
    }

    /// A `latest_config_measurements.json` that will not parse used to
    /// read as an empty one, and the device row's model, firmware and
    /// timezone were overwritten with nothing.
    #[tokio::test]
    async fn an_unreadable_description_leaves_the_device_row_alone() {
        let e = env().await;
        let json = e.root.join(LATEST_JSON);
        std::fs::write(&json, latest_json("KITCHEN01", "kitchen")).unwrap();
        std::fs::write(
            e.root.join("202609_AirVisual_values.txt"),
            format!("{HEADER}{}", line(1788220836, "1.0", "425")),
        )
        .unwrap();
        fetch(opts(&e, kitchen(&e))).await.unwrap();

        std::fs::write(&json, r#"{"serial_number": "KITCH"#).unwrap();
        let s = fetch(opts(&e, kitchen(&e))).await.unwrap();
        assert_eq!(s.errors, 1);
        let tz: Option<String> = sqlx::query_scalar("SELECT timezone FROM airvisual_devices")
            .fetch_one(e.db.pool())
            .await
            .unwrap();
        assert_eq!(tz.as_deref(), Some("Europe/Zurich"));
        assert_eq!(
            keys(&problems(e.db.pool()).await),
            ["record:airvisual_files:KITCHEN01/latest_config_measurements.json"]
        );

        std::fs::write(&json, latest_json("KITCHEN01", "kitchen")).unwrap();
        fetch(opts(&e, kitchen(&e))).await.unwrap();
        assert!(problems(e.db.pool()).await.is_empty());
        e.db.close().await;
    }

    /// A device that cannot be read this run has no verdict on its files:
    /// the rows it had stay until it is read again.
    #[tokio::test]
    async fn a_device_that_cannot_be_read_keeps_its_file_rows() {
        let e = env().await;
        let json = e.root.join(LATEST_JSON);
        std::fs::write(&json, r#"{"serial_number": "KITCH"#).unwrap();
        fetch(opts(&e, kitchen(&e))).await.unwrap();
        let row = "record:airvisual_files:KITCHEN01/latest_config_measurements.json";
        assert_eq!(keys(&problems(e.db.pool()).await), [row]);

        let away = e.root.with_file_name("airvisual-away");
        std::fs::rename(&e.root, &away).unwrap();
        for devices in [kitchen(&e), vec![device(&e.root, None, None)]] {
            fetch(opts(&e, devices)).await.unwrap();
            let rows = problems(e.db.pool()).await;
            assert!(keys(&rows).contains(&row), "{rows:?}");
        }

        std::fs::rename(&away, &e.root).unwrap();
        std::fs::write(&json, latest_json("KITCHEN01", "kitchen")).unwrap();
        fetch(opts(&e, kitchen(&e))).await.unwrap();
        assert!(problems(e.db.pool()).await.is_empty());
        e.db.close().await;
    }

    /// A line that will not parse costs that line; the file is stamped,
    /// so the loss is a row on the file until it is read again.
    #[tokio::test]
    async fn lines_that_do_not_parse_are_the_files_problem_until_it_changes() {
        let e = env().await;
        let f = e.root.join("archive1/202501_AirVisual_values.txt");
        std::fs::write(
            &f,
            format!(
                "{HEADER}{}2025/01/06;18:41:10;17361;3.0;\n",
                line(1736185260, "3.0", "500")
            ),
        )
        .unwrap();
        let s = fetch(opts(&e, kitchen(&e))).await.unwrap();
        assert_eq!((s.errors, s.bad_lines, s.samples), (0, 1, 1));
        let rows = problems(e.db.pool()).await;
        assert_eq!(
            keys(&rows),
            ["file:airvisual/export/KITCHEN01:archive1/202501_AirVisual_values.txt"]
        );
        assert!(rows[0].1.contains("line 3"), "{rows:?}");

        std::fs::write(&f, format!("{HEADER}{}", line(1736185260, "3.0", "500"))).unwrap();
        fetch(opts(&e, kitchen(&e))).await.unwrap();
        assert!(problems(e.db.pool()).await.is_empty());
        e.db.close().await;
    }
}
