//! Ingest the TNG account through playback, render it, and check every
//! document: the overview's weight plot, table and device rows, the
//! metric pages, the daily log, the activities and the records — and
//! that an unchanged store renders the same rows the second time.

use std::path::{Path, PathBuf};

use datalib_etl::control::DownloadControl;
use datalib_etl::progress::Progress;
use datalib_etl_garmin::ingest::{db_path_for, fetch, FetchOptions, RawDb};
use datalib_etl_garmin::synthesize::GarminSynth;
use datalib_etl_garmin_config::GarminApi;
use datalib_etl_garmin_render::render::parse::parse;
use datalib_etl_garmin_render::render::render::{
    activity_uuid, daily_log_uuid, document_uuid, listings_uuid, render_all,
};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::RawRange;
use datalib_etl_web::playback;
use datalib_etl_web::synthesize::Synthesizer;

const SOURCE: &str = "garmin";

fn spec_path() -> PathBuf {
    let rel = "datalib/backend/etl/providers/garmin/tests/fixtures/garmin_tng/tng.json";
    if Path::new(rel).exists() {
        return PathBuf::from(rel);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../garmin/tests/fixtures/garmin_tng/tng.json")
}

async fn ingest(raw: &Path, playback: &Path) {
    GarminSynth::new(spec_path()).synthesize(playback).unwrap();
    let db = RawDb::open(&db_path_for(raw)).await.unwrap();
    let download = fetch(FetchOptions {
        db: db.clone(),
        latchkey: Default::default(),
        api: GarminApi {
            since: Some("2369-04-01".into()),
            ..Default::default()
        },
        today: chrono::NaiveDate::from_ymd_opt(2369, 4, 15).unwrap(),
        progress: Progress::noop(),
        control: DownloadControl::default(),
        sealer: None,
    });
    let s = playback::scope(playback, download).await.unwrap();
    assert_eq!(s.errors, 0, "{}", s.line());
    // The ingest step commits at the end of its run; do the same here so
    // the render's pin has a HEAD to read at.
    datalib_etl::doltlite_raw::commit_run(db.pool(), "test ingest")
        .await
        .unwrap();
    db.close().await;
}

fn render_once(
    raw: &Path,
    root: &Path,
    pin: Option<&str>,
) -> (Vec<RenderedMarkdown>, Option<String>) {
    let mut emitted = Vec::new();
    let range = RawRange {
        pin,
        ..RawRange::cold()
    };
    let parsed = parse(raw, range).unwrap();
    let mut on_doc = |md: RenderedMarkdown| {
        emitted.push(md);
        Ok(())
    };
    render_all(&parsed, root, SOURCE, &Progress::noop(), &mut on_doc).unwrap();
    (emitted, parsed.head.clone())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn renders_every_document_then_renders_an_unchanged_store_the_same() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let raw = root.join(SOURCE).join("ingest");
    std::fs::create_dir_all(&raw).unwrap();
    ingest(&raw, &root.join("playback")).await;

    let (emitted, cursor) = render_once(&raw, root, None);
    let titles: Vec<&str> = emitted
        .iter()
        .map(|d| d.rows.iter().find(|r| r.is_document).unwrap())
        .map(|r| r.conversation_name.as_deref().unwrap())
        .collect();
    assert_eq!(
        titles,
        [
            "Garmin — Jean-Luc Picard",
            "Sleep — Jean-Luc Picard",
            "Heart rate and HRV — Jean-Luc Picard",
            "Stress and Body Battery — Jean-Luc Picard",
            "Steps and activity — Jean-Luc Picard",
            "Daily log — Jean-Luc Picard",
            "Holodeck run: Dixon Hill",
            "Cycling: Deck 12 to Engineering",
            "Records and badges — Jean-Luc Picard",
        ],
        "the TNG account has no respiration, SpO2 or training data, so those pages are left out"
    );
    for d in &emitted {
        assert_eq!(
            d.bucket_key.as_deref(),
            Some(document_uuid(SOURCE).as_str()),
            "every document is in the one bucket"
        );
        let doc_rows: Vec<_> = d.rows.iter().filter(|r| r.is_document).collect();
        assert_eq!(doc_rows.len(), 1);
        assert!(
            doc_rows[0].item_count.is_some(),
            "the render store refuses a document row with no count"
        );
    }
    let doc = &emitted[0];
    assert_eq!(doc.markdown_uuid, document_uuid(SOURCE));
    assert_eq!(doc.rows.len(), 2, "1 page row + 1 device row");
    let page_row = doc.rows.iter().find(|r| r.kind == "Garmin Weight").unwrap();
    assert_eq!(page_row.author.as_deref(), Some("Jean-Luc Picard"));
    assert!(page_row.preview.contains("77.6 kg"), "{}", page_row.preview);
    assert!(
        page_row
            .created_at
            .as_deref()
            .is_some_and(|t| t.ends_with("+00:00")),
        "{:?}",
        page_row.created_at
    );
    let device_row = doc.rows.iter().find(|r| r.kind == "Garmin Device").unwrap();
    assert_eq!(device_row.channel.as_deref(), Some("Forerunner 265"));

    let page_dir = root.join(SOURCE).join("render_markdown");
    let md = std::fs::read_to_string(page_dir.join("index.md")).unwrap();
    assert!(md.contains(">Garmin — Jean-Luc Picard</h1>"), "{md}");
    assert!(md.contains("**77.6 kg**"), "{md}");
    assert!(md.contains("<iframe src=\"plots/weight.html\""), "{md}");
    assert!(
        md.contains("| 2369-04-14"),
        "table row for the latest weigh-in:\n{md}"
    );
    assert!(md.contains("INDEX_SCALE"), "{md}");
    assert!(md.contains("### Forerunner 265"), "{md}");
    assert!(md.contains("| `daily_summary` | 15 | 2 |"), "{md}");
    assert!(md.contains("| Activities | 2 |"), "{md}");
    assert!(
        !md.contains("test ingest"),
        "commit messages must not reach the page:\n{md}"
    );

    let plot = std::fs::read_to_string(page_dir.join("plots/weight.html")).unwrap();
    assert!(plot.contains("77.6"), "{plot}");
    assert!(
        plot.contains("12600168300000"),
        "epoch ms inlined for the date axis"
    );
    assert!(
        plot.contains("Body fat"),
        "body fat from the payload draws on y2"
    );

    let sleep = std::fs::read_to_string(page_dir.join("sleep.md")).unwrap();
    assert!(
        sleep.contains("**2369-04-14:** Sleep score 84 · Asleep 7 h 20 min"),
        "{sleep}"
    );
    assert!(
        sleep.contains("<iframe src=\"plots/sleep.html\""),
        "{sleep}"
    );
    assert!(sleep.contains("| 2369-04-14 | 84 | 7:20 |"), "{sleep}");
    assert!(page_dir.join("plots/sleep.html").exists());

    let log = emitted
        .iter()
        .find(|d| d.markdown_uuid == daily_log_uuid(SOURCE))
        .unwrap();
    let days: Vec<&str> = log
        .rows
        .iter()
        .filter(|r| r.kind == "Garmin Day")
        .map(|r| r.upstream_id.as_deref().unwrap())
        .collect();
    assert_eq!(days, ["2369-04-14", "2369-04-13"], "newest first");
    let day = log.rows.iter().find(|r| r.kind == "Garmin Day").unwrap();
    assert!(
        day.preview.starts_with(
            "2369-04-14 Asleep 7 h 20 min · Sleep score 84 · Resting HR 48 bpm · \
             HRV last night 61 ms · Steps 14,872 · Avg stress 29"
        ),
        "the headline leads, so the cut-short preview shows it: {}",
        day.preview
    );

    let run = emitted
        .iter()
        .find(|d| {
            d.rows
                .iter()
                .any(|r| r.upstream_id.as_deref() == Some("17010413001"))
        })
        .unwrap();
    let run_row = &run.rows[0];
    assert_eq!(run_row.kind, "Garmin Activity");
    assert_eq!(run_row.channel.as_deref(), Some("Running"));
    assert_eq!(
        run_row.created_at.as_deref(),
        Some("2369-04-13T06:30:00+00:00")
    );
    assert!(
        run_row.preview.contains("Distance 8.05 km"),
        "{}",
        run_row.preview
    );
    assert_eq!(
        run.markdown_uuid,
        activity_uuid(SOURCE, "17010413001", Some(12600081000000))
    );
    let run_md = std::fs::read_to_string(&run.md_path).unwrap();
    assert!(run_md.contains("| Pace | 5:00 /km |"), "{run_md}");
    assert!(run_md.contains("| Device | Forerunner 265 |"), "{run_md}");

    let records = emitted
        .iter()
        .find(|d| d.markdown_uuid == listings_uuid(SOURCE))
        .unwrap();
    let kinds: Vec<&str> = records.rows.iter().map(|r| r.kind.as_str()).collect();
    assert_eq!(
        kinds,
        [
            "Garmin Records",
            "Garmin Record",
            "Garmin Badge",
            "Garmin Gear",
            "Garmin Workout"
        ]
    );
    let pr = &records.rows[1];
    assert!(
        pr.preview
            .contains("Fastest 5 km 21:30 · Running · in Holodeck run: Dixon Hill"),
        "{}",
        pr.preview
    );

    // Unchanged store, read at the same commit: identical rows, so the
    // render store records no change.
    let cursor = cursor.expect("a successful render pins the commit it consumed");
    let (again, cursor_2) = render_once(&raw, root, Some(&cursor));
    assert_eq!(cursor_2.as_deref(), Some(cursor.as_str()));
    let texts = |docs: &[RenderedMarkdown]| {
        docs.iter()
            .flat_map(|d| d.rows.iter())
            .map(|r| (r.uuid.clone(), r.preview.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(texts(&again), texts(&emitted));
}

/// The TNG account's time series land in a time-series store, and a
/// second render of an unchanged raw store writes nothing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn time_series_are_stored_then_left_alone_when_nothing_moved() {
    use datalib_etl_garmin_render::render::time_series;
    use datalib_time_series_schema::TimeKind;

    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let raw = root.join(SOURCE).join("ingest");
    ingest(&raw, &root.join("playback")).await;

    let (head, all) = time_series::read(&raw, SOURCE, None).unwrap();
    assert!(head.is_some());
    let parsed = parse(&raw, RawRange::cold()).unwrap();
    let weight = all.iter().find(|s| s.metric == "weight").unwrap();
    assert_eq!(weight.time_kind, TimeKind::Instant);
    assert_eq!(weight.points.len(), parsed.weigh_ins.len());
    let resting = all.iter().find(|s| s.metric == "resting_hr").unwrap();
    assert_eq!(
        (resting.time_kind, resting.unit.as_str()),
        (TimeKind::Day, "bpm")
    );
    assert!(!resting.points.is_empty());
    let ids: std::collections::BTreeSet<&str> =
        all.iter().map(|s| s.time_series_id.as_str()).collect();
    assert_eq!(ids.len(), all.len());

    let store = datalib_etl_time_series::store::open(
        &datalib_etl::layout::render_time_series_db(root, SOURCE),
        datalib_etl::doltlite_raw::StoreKind::TimeSeries,
    )
    .await
    .unwrap();
    let scope = std::collections::BTreeSet::from([SOURCE]);
    let first = datalib_etl_time_series::apply(&store, &all, &scope)
        .await
        .unwrap();
    assert_eq!(first.written, all.len());
    let (_, again) = time_series::read(&raw, SOURCE, None).unwrap();
    let second = datalib_etl_time_series::apply(&store, &again, &scope)
        .await
        .unwrap();
    assert_eq!((second.written, second.unchanged), (0, all.len()));
    store.close().await;
}
