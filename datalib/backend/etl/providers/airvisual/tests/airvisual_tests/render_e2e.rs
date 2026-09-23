//! End-to-end render over a doltlite store this test builds itself:
//! seed samples, render, assert the page, append more, render again.

use std::path::Path;

use datalib_etl::progress::Progress;
use datalib_etl_airvisual::ingest::schema_raw::{
    upsert_samples, AirvisualDeviceRow, AirvisualSampleRow,
};
use datalib_etl_airvisual::ingest::{db_path_for, RawDb};
use datalib_etl_airvisual_render::render::parse::parse;
use datalib_etl_airvisual_render::render::render::{document_uuid, render_all};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::RawRange;
use sqlx::sqlite::SqlitePool;

const STANZA: &str = "air-cucina";

/// `(device, ts_ms, pm25, co2, temperature)`; `None` is a blank cell.
type Seed = (&'static str, i64, Option<f64>, Option<f64>, Option<f64>);

fn sample(
    device: &str,
    ts_ms: i64,
    pm25: Option<f64>,
    co2: Option<f64>,
    t: Option<f64>,
) -> AirvisualSampleRow {
    AirvisualSampleRow {
        device_id: device.to_string(),
        ts_ms,
        pm25_ugm3: pm25,
        pm10_ugm3: None,
        pm1_ugm3: None,
        aqi_us: None,
        aqi_cn: None,
        outdoor_aqi_us: None,
        outdoor_aqi_cn: None,
        temperature_c: t,
        humidity_pct: None,
        co2_ppm: co2,
        voc_ppb: None,
        source_file: "202609_AirVisual_values.txt".into(),
    }
}

async fn seed(pool: &SqlitePool, rows: &[Seed], devices: &[(&str, &str)]) {
    let device_rows: Vec<AirvisualDeviceRow> = devices
        .iter()
        .map(|(id, name)| AirvisualDeviceRow {
            id: (*id).to_string(),
            name: (*name).to_string(),
            model: Some("30".into()),
            mac_address: None,
            app_version: Some("1.1937".into()),
            system_version: Some("KBG66F85".into()),
            timezone: Some("Europe/Zurich".into()),
            last_ts_ms: None,
        })
        .collect();
    let sample_rows: Vec<AirvisualSampleRow> = rows
        .iter()
        .map(|(d, ts, pm25, co2, t)| sample(d, *ts, *pm25, *co2, *t))
        .collect();

    let mut tx = pool.begin().await.unwrap();
    datalib_etl::bulk::bulk_upsert_entity_in_tx(&mut tx, &device_rows)
        .await
        .unwrap();
    upsert_samples(&mut tx, &sample_rows).await.unwrap();
    tx.commit().await.unwrap();
    datalib_etl::doltlite_raw::commit_run(pool, "test seed")
        .await
        .unwrap();
}

/// Parse at `pin` (HEAD when `None`) and render, the way the processor
/// does once the driver has decided the page is stale. Returns the
/// emitted documents and the commit the pass read at.
fn render_once(
    raw_path: &Path,
    root: &Path,
    pin: Option<&str>,
) -> (Vec<RenderedMarkdown>, Option<String>) {
    let mut emitted = Vec::new();
    let range = RawRange {
        pin,
        ..RawRange::cold()
    };
    let parsed = parse(raw_path, range).unwrap();
    let mut on_doc = |md: RenderedMarkdown| {
        emitted.push(md);
        Ok(())
    };
    render_all(&parsed, root, STANZA, &Progress::noop(), &mut on_doc).unwrap();
    (emitted, parsed.head.clone())
}

#[tokio::test(flavor = "multi_thread")]
async fn renders_one_plot_per_quantity_with_data_then_skips_until_data_lands() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let raw_path = root.join(STANZA).join("raw");
    std::fs::create_dir_all(&raw_path).unwrap();
    let db = RawDb::open(&db_path_for(&raw_path)).await.unwrap();

    // Two Pros; the second has one sensor-off sample (PM blank).
    seed(
        db.pool(),
        &[
            (
                "4133WV2JB9Z",
                1_788_220_836_000,
                Some(1.0),
                Some(425.0),
                Some(23.5),
            ),
            (
                "4133WV2JB9Z",
                1_788_221_736_000,
                Some(2.0),
                Some(430.0),
                Some(23.4),
            ),
            (
                "QKAO9PC1XDJ",
                1_788_220_836_000,
                None,
                Some(610.0),
                Some(21.0),
            ),
        ],
        &[("4133WV2JB9Z", "Cucina"), ("QKAO9PC1XDJ", "Schlafzimmer")],
    )
    .await;

    let (emitted, cursor) = render_once(&raw_path, root, None);
    assert_eq!(emitted.len(), 1, "the whole store renders as one document");
    let doc = &emitted[0];
    assert_eq!(doc.markdown_uuid, document_uuid(STANZA));
    assert_eq!(doc.rows.len(), 3, "1 page row + 2 device rows");
    assert!(
        doc.rows.iter().all(|r| r.provider == "airvisual"),
        "{:?}",
        doc.rows
    );
    assert!(doc.rows.iter().any(|r| r.kind == "Sensor Timeseries"));
    assert_eq!(
        doc.rows
            .iter()
            .filter(|r| r.kind == "Sensor Device")
            .count(),
        2
    );

    let page_dir = root.join(STANZA).join("render_markdown");
    let md = std::fs::read_to_string(page_dir.join("index.md")).unwrap();
    let plots = page_dir.join("plots");

    // Only the quantities with data get a plot; the rest get neither a
    // file nor a broken iframe.
    for key in ["particulates", "co2", "temperature"] {
        assert!(
            plots.join(format!("{key}.html")).is_file(),
            "missing plots/{key}.html"
        );
        assert!(
            md.contains(&format!("<iframe src=\"plots/{key}.html\"")),
            "index.md does not iframe plots/{key}.html:\n{md}"
        );
    }
    for key in ["aqi", "humidity", "voc"] {
        assert!(
            !plots.join(format!("{key}.html")).exists(),
            "plots/{key}.html should not exist"
        );
        assert!(!md.contains(&format!("plots/{key}.html")), "{md}");
    }

    // Both devices are series on the one CO2 plot, under their names,
    // not their serials; the sensor-off sample keeps Schlafzimmer off
    // the particulates plot.
    let co2 = std::fs::read_to_string(plots.join("co2.html")).unwrap();
    assert!(co2.contains("\"name\":\"Cucina\""), "{co2}");
    assert!(co2.contains("\"name\":\"Schlafzimmer\""), "{co2}");
    let pm = std::fs::read_to_string(plots.join("particulates.html")).unwrap();
    assert!(pm.contains("Cucina (PM2.5)"), "{pm}");
    assert!(
        !pm.contains("Schlafzimmer"),
        "a NULL cell must not become a point:\n{pm}"
    );

    assert!(md.contains("## Devices"), "{md}");
    assert!(md.contains("### Cucina"), "{md}");
    assert!(md.contains("serial `4133WV2JB9Z`"), "{md}");
    assert!(md.contains("firmware 1.1937 / KBG66F85"), "{md}");
    assert!(md.contains("clock in Europe/Zurich"), "{md}");
    assert!(md.contains("## Store"), "{md}");
    assert!(
        !md.contains("test seed"),
        "the doltlite commit log is back in the rendered page, which makes the render nondeterministic:\n{md}"
    );

    let md_1 = md.clone();

    assert_eq!(
        doc.bucket_key.as_deref(),
        Some(document_uuid(STANZA).as_str()),
        "the page declares itself as one bucket"
    );
    assert!(
        doc.upstream_cursor.is_none(),
        "the raw HEAD is per-run and stays off the row"
    );

    // ---- second render, nothing appended, read at the same commit ----
    let cursor = cursor.expect("a successful render pins the commit it consumed");
    let (emitted, cursor_2) = render_once(&raw_path, root, Some(&cursor));
    assert_eq!(cursor_2.as_deref(), Some(cursor.as_str()));
    assert_eq!(emitted.len(), 1);
    assert_eq!(
        std::fs::read_to_string(page_dir.join("index.md")).unwrap(),
        md_1,
        "an unchanged store must render the same page, so the store records no change"
    );

    // ---- third render, one new sample ---------------------------------
    seed(
        db.pool(),
        &[(
            "Cucina",
            1_788_222_636_000,
            Some(3.0),
            Some(440.0),
            Some(23.3),
        )],
        &[],
    )
    .await;
    let (emitted, cursor_3) = render_once(&raw_path, root, None);
    assert_ne!(
        cursor_3,
        Some(cursor),
        "the store moved, so the cursor must too"
    );
    assert_eq!(emitted.len(), 1, "an appended sample must re-render");
    assert!(
        std::fs::read_to_string(plots.join("co2.html"))
            .unwrap()
            .contains("1788222636000"),
        "the new sample is missing from the CO2 plot"
    );
    db.close().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_store_renders_a_page_without_plots() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let raw_path = root.join(STANZA).join("raw");
    std::fs::create_dir_all(&raw_path).unwrap();
    let db = RawDb::open(&db_path_for(&raw_path)).await.unwrap();
    seed(db.pool(), &[], &[("4133WV2JB9Z", "Cucina")]).await;

    let (emitted, _) = render_once(&raw_path, root, None);
    assert_eq!(emitted.len(), 1);
    let md = std::fs::read_to_string(root.join(STANZA).join("render_markdown").join("index.md"))
        .unwrap();
    assert!(md.contains("nothing to plot"), "{md}");
    assert!(md.contains("no samples yet"), "{md}");
    assert!(!root
        .join(STANZA)
        .join("render_markdown")
        .join("plots")
        .join("co2.html")
        .exists());
    db.close().await;
}
