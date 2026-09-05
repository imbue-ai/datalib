//! End-to-end render over a doltlite store this test builds itself:
//! seed readings, render, assert the page, append more, render again.
//!
//! The load-bearing assertion is the **skip**. YoLink's whole
//! incrementality story is "HEAD unchanged → don't re-render", and a
//! test that only checks the happy path would pass just as well against
//! a renderer that re-rendered every run — so the skip is checked by
//! watching a re-render fail to happen (no doc emitted, and the file's
//! mtime-independent content stamp unchanged), and then checked in the
//! other direction by appending a reading and watching the render come
//! back. Both halves are needed: on its own, "returns UpToDate" is
//! satisfied by a renderer that never renders at all.

use std::path::Path;

use datalib_etl::grid_index::RenderedMarkdown;
use datalib_etl::progress::Progress;
use datalib_etl::render_cursor;
use datalib_etl_yolink::download::schema_raw::{YolinkDeviceRow, YolinkReadingRow};
use datalib_etl_yolink::download::{db_path_for, RawDb};
use datalib_etl_yolink::render::parse::{parse, Parsed};
use datalib_etl_yolink::render::render::{cursor_params, document_uuid, render_all};
use sqlx::sqlite::SqlitePool;

const STANZA: &str = "yolink";

/// Seed a device row plus its readings, then commit so `dolt_log()` has
/// a HEAD to hand back.
async fn seed(pool: &SqlitePool, rows: &[(&str, &str, i64, f64)], devices: &[(&str, &str)]) {
    let now = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    let device_rows: Vec<YolinkDeviceRow> = devices
        .iter()
        .map(|(name, kind)| YolinkDeviceRow {
            id: (*name).to_string(),
            family_device_id: "0123456789abcdef0123456789abcdef".into(),
            kind: (*kind).to_string(),
            start_ms: 1_700_000_000_000,
        })
        .collect();
    let reading_rows: Vec<YolinkReadingRow> = rows
        .iter()
        .map(|(device, metric, ts, value)| {
            YolinkReadingRow::new(device, *ts, metric, *value, "{}".into())
        })
        .collect();

    let mut tx = pool.begin().await.unwrap();
    if !device_rows.is_empty() {
        datalib_etl::bulk::bulk_upsert_in_tx(&mut tx, &device_rows, &now)
            .await
            .unwrap();
    }
    if !reading_rows.is_empty() {
        datalib_etl::bulk::bulk_upsert_in_tx(&mut tx, &reading_rows, &now)
            .await
            .unwrap();
    }
    tx.commit().await.unwrap();
    sqlx::query("SELECT dolt_commit('-Am', 'test seed')")
        .execute(pool)
        .await
        .unwrap();
}

/// Run the render processor's inner loop the way `processor.rs` does:
/// read the cursor, parse, and render only when the store moved.
/// Returns the emitted documents (empty when the render was skipped).
fn render_once(raw_path: &Path, root: &Path) -> Vec<RenderedMarkdown> {
    let cursor_path = render_cursor::cursor_path(root, STANZA);
    let cursor = render_cursor::read_for_params(&cursor_path, &cursor_params()).unwrap();
    let mut emitted = Vec::new();
    match parse(
        raw_path,
        cursor.as_ref().map(|c| c.last_rendered_hash.as_str()),
    )
    .unwrap()
    {
        Parsed::UpToDate { .. } => {}
        Parsed::Fresh(parsed) => {
            let mut on_doc = |md: RenderedMarkdown| {
                emitted.push(md);
                Ok(())
            };
            render_all(&parsed, root, STANZA, &Progress::noop(), &mut on_doc).unwrap();
        }
    }
    emitted
}

#[tokio::test(flavor = "multi_thread")]
async fn renders_a_page_with_one_plot_per_quantity_then_skips_until_data_lands() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let raw_path = root.join(STANZA).join("raw");
    std::fs::create_dir_all(&raw_path).unwrap();
    let db = RawDb::open(&db_path_for(&raw_path)).await.unwrap();

    // Two THSensors reporting °C + %RH, and a water meter reporting
    // gallons in both a per-sample and a cumulative metric.
    seed(
        db.pool(),
        &[
            ("main_fridge", "temperature_c", 1_781_481_609_000, 3.5),
            ("main_fridge", "temperature_c", 1_781_481_669_000, 3.7),
            ("main_fridge", "humidity_pct", 1_781_481_609_000, 42.0),
            (
                "basement_freezer",
                "temperature_c",
                1_781_481_609_000,
                -18.4,
            ),
            ("basement_freezer", "humidity_pct", 1_781_481_609_000, 70.0),
            ("water_valve", "water_meter_gal", 1_781_481_609_000, 100.0),
            (
                "water_valve",
                "water_consumption_gal",
                1_781_481_609_000,
                2.0,
            ),
        ],
        &[
            ("basement_freezer", "temperature_humidity"),
            ("main_fridge", "temperature_humidity"),
            ("water_valve", "watermeter"),
        ],
    )
    .await;

    // ---- first render -------------------------------------------------
    let emitted = render_once(&raw_path, root);
    assert_eq!(emitted.len(), 1, "the whole store renders as one document");
    let doc = &emitted[0];
    assert_eq!(doc.markdown_uuid, document_uuid(STANZA));
    // One row for the page + one per device.
    assert_eq!(doc.rows.len(), 4, "1 page row + 3 device rows");
    assert!(doc.rows.iter().any(|r| r.kind == "Sensor Timeseries"));
    assert_eq!(
        doc.rows
            .iter()
            .filter(|r| r.kind == "Sensor Device")
            .count(),
        3
    );

    let page_dir = root.join(STANZA).join("rendered_md");
    let md = std::fs::read_to_string(page_dir.join("index.md")).unwrap();
    let plots = page_dir.join("plots");

    // One plot per quantity present in the store, each iframed by a
    // RELATIVE src so the page works off disk as well as through the
    // API's asset route.
    for key in ["temperature", "humidity", "volume"] {
        assert!(
            plots.join(format!("{key}.html")).is_file(),
            "missing plots/{key}.html"
        );
        assert!(
            md.contains(&format!("<iframe src=\"plots/{key}.html\"")),
            "index.md does not iframe plots/{key}.html:\n{md}"
        );
    }

    // Both devices land as series on the ONE temperature plot.
    let temp = std::fs::read_to_string(plots.join("temperature.html")).unwrap();
    assert!(temp.contains("main_fridge"), "{temp}");
    assert!(temp.contains("basement_freezer"), "{temp}");
    assert!(
        !temp.contains("water_valve"),
        "volume series must not be on the temperature plot"
    );

    // Gallons are converted to litres, and the cumulative meter draws on
    // the secondary axis so it doesn't flatten per-sample consumption.
    let volume = std::fs::read_to_string(plots.join("volume.html")).unwrap();
    assert!(
        volume.contains("378.5411784"),
        "100 gal should plot as 378.5411784 L:\n{volume}"
    );
    assert!(
        volume.contains("7.570823568"),
        "2 gal should plot as 7.570823568 L:\n{volume}"
    );
    assert!(
        !volume.contains("\"y\":[100"),
        "raw gallons leaked into the plot"
    );
    assert!(
        volume.contains(r#""yaxis":"y2""#),
        "meter total is not on y2"
    );
    assert!(volume.contains("water_valve (meter total)"), "{volume}");
    assert!(volume.contains("water_valve (consumption)"), "{volume}");

    // The non-timeseries half: devices, their kinds, and store
    // provenance — and NOT the per-device read credential.
    assert!(md.contains("## Devices"), "{md}");
    assert!(md.contains("temperature_humidity"), "{md}");
    assert!(md.contains("watermeter"), "{md}");
    assert!(md.contains("## Store"), "{md}");
    assert!(md.contains("| Commits |"), "store counts missing:\n{md}");
    // Counts yes; the doltlite HEAD hash and the per-commit hashes and
    // wall-clock dates no. `test seed` is this test's own commit message,
    // so it appears in the page only if the commit log is being rendered.
    // That log is stamped from the wall clock, which made this one file
    // the reason a rendered tree was never byte-identical to its previous
    // self — and so re-ran the fixture's ~90s CPU-only embed on CI for
    // changes that altered nothing the embedder reads. See
    // `render_store_section` and `tests/fixtures/tar_qmd.py`.
    assert!(
        !md.contains("test seed"),
        "the doltlite commit log is back in the rendered page, which makes \
         the render nondeterministic:\n{md}"
    );
    assert!(
        !md.contains("0123456789abcdef0123456789abcdef"),
        "family_device_id (a device read credential) leaked into the page"
    );

    let fingerprint_1 = doc.source_fingerprint.clone();
    let md_1 = md.clone();

    // ---- second render, nothing appended ------------------------------
    let cursor = render_cursor::read(&render_cursor::cursor_path(root, STANZA))
        .unwrap()
        .expect("cursor written after a successful render");
    assert_eq!(
        cursor.params,
        Some(cursor_params()),
        "the cursor must record the render version so a bump re-renders"
    );

    let emitted = render_once(&raw_path, root);
    assert!(
        emitted.is_empty(),
        "HEAD did not move, so nothing should have been rendered"
    );
    assert_eq!(
        std::fs::read_to_string(page_dir.join("index.md")).unwrap(),
        md_1,
        "the page was rewritten despite an unchanged store"
    );

    // ---- third render, one new reading --------------------------------
    seed(
        db.pool(),
        &[("main_fridge", "temperature_c", 1_781_481_729_000, 4.1)],
        &[],
    )
    .await;
    let emitted = render_once(&raw_path, root);
    assert_eq!(emitted.len(), 1, "an appended reading must re-render");
    assert_ne!(
        emitted[0].source_fingerprint, fingerprint_1,
        "the re-rendered page must not reuse the previous fingerprint"
    );
    let md_3 = std::fs::read_to_string(page_dir.join("index.md")).unwrap();
    assert_ne!(
        md_3, md_1,
        "the page content should reflect the new reading"
    );
    assert!(
        std::fs::read_to_string(plots.join("temperature.html"))
            .unwrap()
            .contains("1781481729000"),
        "the new sample is missing from the temperature plot"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_metric_with_no_unit_mapping_fails_loudly() {
    // A new sensor kind must not render a page that looks complete and
    // silently omits its series.
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let raw_path = root.join(STANZA).join("raw");
    std::fs::create_dir_all(&raw_path).unwrap();
    let db = RawDb::open(&db_path_for(&raw_path)).await.unwrap();
    seed(
        db.pool(),
        &[("gauge", "pressure_psi", 1_781_481_609_000, 14.7)],
        &[("gauge", "pressure")],
    )
    .await;

    let Parsed::Fresh(parsed) = parse(&raw_path, None).unwrap() else {
        panic!("cold start must not report UpToDate");
    };
    let err = render_all(&parsed, root, STANZA, &Progress::noop(), &mut |_| Ok(()))
        .unwrap_err()
        .to_string();
    assert!(err.contains("pressure_psi"), "{err}");
    assert!(err.contains("units.rs"), "{err}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_empty_store_renders_a_page_without_plots() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let raw_path = root.join(STANZA).join("raw");
    std::fs::create_dir_all(&raw_path).unwrap();
    let db = RawDb::open(&db_path_for(&raw_path)).await.unwrap();
    seed(db.pool(), &[], &[("main_fridge", "temperature_humidity")]).await;

    let emitted = render_once(&raw_path, root);
    assert_eq!(emitted.len(), 1);
    let md = std::fs::read_to_string(root.join(STANZA).join("rendered_md/index.md")).unwrap();
    assert!(md.contains("nothing to plot"), "{md}");
    assert!(md.contains("main_fridge"), "{md}");
    assert!(
        !root
            .join(STANZA)
            .join("rendered_md/plots/temperature.html")
            .exists(),
        "an empty quantity must not produce an empty plot file"
    );
}

/// Build a store at `raw_path`, render it cold, and return the emitted
/// document's fingerprint.
async fn fingerprint_of(root: &Path, rows: &[(&str, &str, i64, f64)]) -> String {
    let raw_path = root.join(STANZA).join("raw");
    std::fs::create_dir_all(&raw_path).unwrap();
    let db = RawDb::open(&db_path_for(&raw_path)).await.unwrap();
    seed(db.pool(), rows, &[("fridge", "temperature_humidity")]).await;

    let Parsed::Fresh(parsed) = parse(&raw_path, None).unwrap() else {
        panic!("cold start must not report UpToDate");
    };
    let mut out = None;
    render_all(
        &parsed,
        root,
        STANZA,
        &Progress::noop(),
        &mut |md: RenderedMarkdown| {
            out = Some(md.source_fingerprint.clone());
            Ok(())
        },
    )
    .unwrap();
    out.expect("a document should have been emitted")
}

#[tokio::test(flavor = "multi_thread")]
async fn the_fingerprint_hashes_the_readings_not_the_store() {
    // Two stores with identical readings but independent histories —
    // different `dolt_log()` HEADs, because doltlite stamps its bootstrap
    // commits with the wall clock and hashes chain.
    //
    // The contract is that `source_fingerprint` hashes the
    // upstream payload, so these must agree. Folding HEAD in instead
    // (which is tempting, since HEAD is what gates the render) makes the
    // `markdowns` row move every time the store is rebuilt from scratch,
    // and makes two mirrors of the same data disagree.
    let rows: &[(&str, &str, i64, f64)] = &[
        ("fridge", "temperature_c", 1_781_481_609_000, 3.5),
        ("fridge", "temperature_c", 1_781_481_669_000, 3.7),
    ];
    let a = tempfile::tempdir().unwrap();
    let b = tempfile::tempdir().unwrap();
    let fp_a = fingerprint_of(a.path(), rows).await;
    let fp_b = fingerprint_of(b.path(), rows).await;
    assert_eq!(
        fp_a, fp_b,
        "same readings must fingerprint the same regardless of store identity"
    );

    // ...and it is genuinely sensitive to the payload. A corrected
    // historical value is the case a shape-only hash (counts +
    // timestamps) would miss, and yolink re-fetches overlapping windows
    // precisely so corrections land — so this is a real path, not a
    // hypothetical.
    let c = tempfile::tempdir().unwrap();
    let corrected: &[(&str, &str, i64, f64)] = &[
        ("fridge", "temperature_c", 1_781_481_609_000, 3.5),
        ("fridge", "temperature_c", 1_781_481_669_000, 3.9), // was 3.7
    ];
    let fp_c = fingerprint_of(c.path(), corrected).await;
    assert_ne!(
        fp_a, fp_c,
        "a corrected value with the same count and timestamps must change \
         the fingerprint, or the Load step will skip a changed document"
    );
}
