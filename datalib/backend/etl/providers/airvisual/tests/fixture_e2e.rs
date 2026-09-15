//! Ingest the checked-in TNG fixture — two Pros' data folders, with an
//! archive, a pre-clock file, a `corrupt_`/`restored_` pair and a live
//! month ending in NULs — then render it and read the page back.

use std::path::PathBuf;

use datalib_etl::control::DownloadControl;
use datalib_etl::fingerprint_cache::FingerprintCache;
use datalib_etl::progress::Progress;
use datalib_etl_airvisual::ingest::{db_path_for, fetch, FetchOptions, RawDb};
use datalib_etl_airvisual_config::AirvisualDevice;
use datalib_etl_airvisual_render::render::parse::{inputs, parse};
use datalib_etl_airvisual_render::render::render::render_all;
use datalib_etl_render::grid_index::RenderedMarkdown;
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
    assert_eq!((s.devices, s.files, s.errors, s.bad_lines), (2, 7, 0, 0));
    assert_eq!(s.clock_unset, 2, "the 1970 file's two lines");
    // ten-forward: 20 + 12 placeable lines; sickbay: 24 + 10 + 4 + 6,
    // of which the corrupt/restored pair share one timestamp.
    assert_eq!(s.lines, 2 + 20 + 12 + 24 + 10 + 4 + 6);
    assert_eq!(s.samples, s.lines - 2);
    let pool = db.pool();
    assert_eq!(
        count(
            pool,
            "SELECT COUNT(*) FROM airvisual_samples WHERE device_id = 'TENFWD10X'"
        )
        .await,
        32
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
    assert_eq!((again.files, again.files_skipped, again.samples), (7, 7, 0));

    // Commit, so the render has a HEAD to pin.
    sqlx::query("SELECT dolt_commit('-Am', 'fixture ingest')")
        .execute(pool)
        .await
        .unwrap();
    db.close().await;

    let parsed = parse(&raw_path, RawRange::cold()).unwrap();
    assert!(parsed.head.is_some(), "a committed store pins a commit");
    assert_eq!(
        inputs().len(),
        3,
        "the page declares the devices, samples and files tables whole"
    );
    assert_eq!(parsed.devices.len(), 2);
    assert_eq!(parsed.sample_count, 75);
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
    let md = std::fs::read_to_string(page_dir.join("index.md")).unwrap();
    assert!(md.contains("### Ten Forward"), "{md}");
    assert!(md.contains("### Sickbay"), "{md}");
    assert!(md.contains("serial `TENFWD10X`"), "{md}");
    let co2 = std::fs::read_to_string(page_dir.join("plots").join("co2.html")).unwrap();
    assert!(
        co2.contains("\"name\":\"Ten Forward\""),
        "legends carry names, not serials"
    );
    assert!(!co2.contains("TENFWD10X"), "{co2}");
}
