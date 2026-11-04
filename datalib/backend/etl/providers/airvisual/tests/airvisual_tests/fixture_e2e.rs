//! Ingest the checked-in TNG fixture — two Pros' data folders, with an
//! archive, a pre-clock file, a `corrupt_`/`restored_` pair and a live
//! month ending in NULs — then render it and read the page back.

use std::path::PathBuf;

use datalib_etl::control::DownloadControl;
use datalib_etl::progress::Progress;
use datalib_etl_airvisual::ingest::{db_path_for, fetch, FetchOptions, RawDb};
use datalib_etl_airvisual_config::AirvisualDevice;
use datalib_etl_airvisual_render::render::parse::{inputs, parse};
use datalib_etl_airvisual_render::render::render::render_all;
use datalib_etl_files::fingerprint_cache::FingerprintCache;
use datalib_etl_render::grid_index::{doc_md_path, RenderedMarkdown};
use datalib_etl_render::inputs::RawRange;

const STANZA: &str = "ship-air";

fn fixture_dir() -> PathBuf {
    if let Ok(d) = std::env::var("AIRVISUAL_FIXTURE_DIR") {
        return PathBuf::from(d);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/airvisual_tng")
}

async fn count(pool: &sqlx::SqlitePool, sql: &'static str) -> i64 {
    sqlx::query_scalar(sql).fetch_one(pool).await.unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn the_tng_fixture_ingests_and_renders() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let raw_path = root.join(STANZA).join("raw");
    std::fs::create_dir_all(&raw_path).unwrap();
    let db = RawDb::open(&db_path_for(&raw_path)).await.unwrap();
    let cache = FingerprintCache::open(&td.path().join("fp.sqlite"))
        .await
        .unwrap();
    let fx = fixture_dir();
    let devices = || {
        vec![
            AirvisualDevice {
                path: fx.join("ten-forward"),
                serial: None,
                name: None,
            },
            AirvisualDevice {
                path: fx.join("sickbay"),
                serial: None,
                name: None,
            },
        ]
    };
    let opts = || FetchOptions {
        db: db.clone(),
        devices: devices(),
        cache: cache.clone(),
        progress: Progress::default(),
        control: DownloadControl::default(),
    };

    let s = fetch(opts()).await.unwrap();
    assert_eq!((s.devices, s.files, s.errors, s.bad_lines), (2, 9, 0, 0));
    assert_eq!(s.clock_unset, 2, "the 1970 file's two lines");
    // ten-forward: 20 + 12 + 3 + 3 placeable lines; sickbay: 24 + 10 + 4 + 6,
    // of which the corrupt/restored pair share one timestamp.
    assert_eq!(s.lines, 2 + 20 + 12 + 3 + 3 + 24 + 10 + 4 + 6);
    assert_eq!(s.samples, s.lines - 2);
    let pool = db.pool();
    assert_eq!(
        count(
            pool,
            "SELECT COUNT(*) FROM airvisual_samples WHERE device_id = 'TENFWD10X'"
        )
        .await,
        38
    );
    assert_eq!(
        count(
            pool,
            "SELECT COUNT(*) FROM airvisual_samples WHERE device_id = 'SICKBAY01'"
        )
        .await,
        43,
        "44 lines, one timestamp shared by the corrupt and restored files"
    );
    assert_eq!(
        count(pool, "SELECT COUNT(*) FROM airvisual_unplaced_samples").await,
        2
    );
    // Identity and name came from each folder's JSON.
    let devs: Vec<(String, String)> =
        sqlx::query_as("SELECT id, name FROM airvisual_devices ORDER BY id")
            .fetch_all(pool)
            .await
            .unwrap();
    assert_eq!(
        devs,
        vec![
            ("SICKBAY01".into(), "Sickbay".into()),
            ("TENFWD10X".into(), "Ten Forward".into())
        ]
    );
    // The restored file's values won the shared timestamp (path order),
    // and its short header left humidity NULL.
    let (pm, rh, src): (Option<f64>, Option<f64>, String) = sqlx::query_as(
        "SELECT pm25_ugm3, humidity_pct, source_file FROM airvisual_samples \
           WHERE device_id = 'SICKBAY01' AND ts_ms = 1725148800000",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!((pm, rh), (Some(1.5), None));
    assert!(src.contains("restored_"), "{src}");
    // A sensor-off line kept only the outdoor index; the -1 line kept its CO2.
    let (pm, out): (Option<f64>, Option<f64>) = sqlx::query_as(
        "SELECT pm25_ugm3, outdoor_aqi_us FROM airvisual_samples \
           WHERE device_id = 'SICKBAY01' AND ts_ms = 1725148810000",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!((pm, out), (None, Some(16.0)));
    let (pm, co2): (Option<f64>, Option<f64>) = sqlx::query_as(
        "SELECT pm25_ugm3, co2_ppm FROM airvisual_samples \
           WHERE device_id = 'SICKBAY01' AND ts_ms = 1725148845000",
    )
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!((pm, co2), (None, Some(470.0)));

    // A second pass reads nothing.
    let again = fetch(opts()).await.unwrap();
    assert_eq!((again.files, again.files_skipped, again.samples), (9, 9, 0));

    // Commit, so the render has a HEAD to pin.
    datalib_etl::doltlite_raw::commit_run(pool, "fixture ingest")
        .await
        .unwrap();
    db.close().await;

    let parsed = parse(&raw_path, RawRange::cold()).unwrap();
    assert!(parsed.head.is_some(), "a committed store pins a commit");
    assert_eq!(
        inputs().len(),
        4,
        "the page declares the devices, samples, clock changes and files tables whole"
    );
    assert_eq!(parsed.devices.len(), 2);
    assert_eq!(parsed.sample_count, 81);
    // Ten Forward's clock went back late: its count of the two stretches
    // interleaves, and real time puts one after the other, at UTC-7 and then
    // UTC-8. CO2 climbs through both, so the order shows.
    let co2 = parsed
        .series
        .iter()
        .find(|s| s.device == "TENFWD10X" && s.metric == "co2_ppm")
        .unwrap();
    let last = co2.ts_ms.len() - 6;
    assert_eq!(
        co2.values[last..],
        [610.0, 620.0, 630.0, 640.0, 650.0, 660.0]
    );
    let at = |s: &str| {
        chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
            .unwrap()
            .and_utc()
            .timestamp_millis()
    };
    assert_eq!(co2.ts_ms[last], at("2024-11-03 09:30:10"));
    assert_eq!(co2.ts_ms[last + 2], at("2024-11-03 10:30:10"));
    assert_eq!(co2.ts_ms[last + 3], at("2024-11-03 10:45:09"));
    let tenfwd = parsed.devices.iter().find(|d| d.id == "TENFWD10X").unwrap();
    assert_eq!(tenfwd.last_ts_ms, Some(at("2024-11-03 11:45:09")));
    let mut emitted = Vec::new();
    let mut on_doc = |md: RenderedMarkdown| {
        emitted.push(md);
        Ok(())
    };
    let summary = render_all(&parsed, root, STANZA, &Progress::noop(), &mut on_doc).unwrap();
    assert_eq!(
        (summary.devices, summary.plots),
        (2, 5),
        "no VOC on these units"
    );
    assert_eq!(emitted.len(), 1);
    assert_eq!(emitted[0].rows.len(), 3);

    let page_dir = root.join(STANZA).join("render_markdown");
    let page = doc_md_path(&page_dir, &emitted[0].markdown_uuid);
    let md = std::fs::read_to_string(&page).unwrap();
    assert!(md.contains("### Ten Forward"), "{md}");
    assert!(md.contains("### Sickbay"), "{md}");
    assert!(md.contains("serial `TENFWD10X`"), "{md}");
    assert_eq!(super::render_e2e::charts(&md).len(), 5);
    let named: Vec<String> =
        datalib_etl_airvisual_render::render::time_series::read(&raw_path, STANZA)
            .unwrap()
            .into_iter()
            .filter(|s| s.name.starts_with("TENFWD10X/"))
            .filter_map(|s| s.about_name)
            .collect();
    assert!(!named.is_empty());
    assert!(
        named.iter().all(|n| n == "Ten Forward"),
        "a device is named, not its serial: {named:?}"
    );
}

/// The fixture stands in for a real device, so it has a real device's clock:
/// every `Timestamp` is its `Date` and `Time` read as UTC, each file lies
/// within the stretch its folder says, and `history.txt` holds nothing a time
/// zone cannot explain. A fixture that drifted from this would test a device
/// that does not exist, which is how the clock went wrong unnoticed.
#[test]
fn the_tng_fixture_has_the_clock_of_a_real_device() {
    use datalib_etl_airvisual::ingest::clock::{clock_model, parse_history};

    let wall = |date: &str, time: &str| {
        let date = chrono::NaiveDate::parse_from_str(date, "%Y/%m/%d")
            .or_else(|_| chrono::NaiveDate::parse_from_str(date, "%m/%d/%Y"))
            .unwrap();
        let time = chrono::NaiveTime::parse_from_str(time, "%H:%M:%S").unwrap();
        date.and_time(time).and_utc().timestamp()
    };
    for device in ["sickbay", "ten-forward"] {
        let dir = fixture_dir().join(device);
        let (changes, bad) =
            parse_history(&std::fs::read_to_string(dir.join("history.txt")).unwrap());
        assert_eq!(bad, Vec::<String>::new());
        let (model, trouble) = clock_model(&changes, None);
        assert_eq!(trouble, Vec::<String>::new(), "{device}");

        let mut folders = vec![(dir.clone(), None, changes.last().map(|c| c.after_wall_s))];
        for (i, c) in changes.iter().enumerate() {
            let from = i.checked_sub(1).map(|p| changes[p].after_wall_s);
            folders.push((
                dir.join(format!("archive{}", c.archive)),
                Some(c.previous_wall_s),
                from,
            ));
        }
        let mut seen = 0;
        for (folder, until, from) in folders {
            for file in std::fs::read_dir(&folder).unwrap() {
                let file = file.unwrap().path();
                let name = file.file_name().unwrap().to_string_lossy().into_owned();
                if !name.ends_with("_AirVisual_values.txt") || name.starts_with("1970") {
                    continue;
                }
                let text = String::from_utf8(std::fs::read(&file).unwrap()).unwrap();
                for line in text.lines().skip(1) {
                    let line = line.trim_matches(char::from(0));
                    let cells: Vec<&str> = line.split(';').collect();
                    if cells.len() < 3 {
                        continue;
                    }
                    let stamp: i64 = cells[2].parse().unwrap();
                    assert_eq!(stamp, wall(cells[0], cells[1]), "{device}/{name}: {line}");
                    assert!(until.is_none_or(|u| stamp <= u), "{device}/{name}: {line}");
                    assert!(from.is_none_or(|f| stamp >= f), "{device}/{name}: {line}");
                    seen += 1;
                }
            }
        }
        assert!(seen > 30, "{device}: {seen} lines is not the fixture");
        assert!(
            model.real_ms("x.txt", 0) != 0,
            "{device}: the last stretch has an offset"
        );
    }
}
