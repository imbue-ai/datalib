//! Ingest the TNG account through playback, render it, and check the
//! page: the weight plot, the table, the device rows, and that an
//! unchanged store renders the same rows the second time.

use std::path::{Path, PathBuf};

use datalib_etl::control::DownloadControl;
use datalib_etl::http::PLAYBACK_ENV;
use datalib_etl::progress::Progress;
use datalib_etl::synthesize::Synthesizer;
use datalib_etl_garmin::auth::Credentials;
use datalib_etl_garmin::ingest::{db_path_for, fetch, FetchOptions, RawDb};
use datalib_etl_garmin::synthesize::GarminSynth;
use datalib_etl_garmin_config::GarminApi;
use datalib_etl_garmin_render::render::parse::parse;
use datalib_etl_garmin_render::render::render::{document_uuid, render_all};
use datalib_etl_render::grid_index::RenderedMarkdown;
use datalib_etl_render::inputs::RawRange;

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
    std::env::set_var(PLAYBACK_ENV, playback);
    let db = RawDb::open(&db_path_for(raw)).await.unwrap();
    let s = fetch(FetchOptions {
        db: db.clone(),
        creds: Credentials::fixed("playback"),
        api: GarminApi {
            since: Some("2369-04-01".into()),
            ..Default::default()
        },
        today: chrono::NaiveDate::from_ymd_opt(2369, 4, 15).unwrap(),
        progress: Progress::noop(),
        control: DownloadControl::default(),
        sealer: None,
    })
    .await
    .unwrap();
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
async fn renders_the_weight_page_then_skips_an_unchanged_store() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path();
    let raw = root.join(SOURCE).join("ingest");
    std::fs::create_dir_all(&raw).unwrap();
    ingest(&raw, &root.join("playback")).await;

    let (emitted, cursor) = render_once(&raw, root, None);
    assert_eq!(emitted.len(), 1, "the whole store is one page");
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

    // Unchanged store, read at the same commit: identical rows, so the
    // render store records no change.
    let cursor = cursor.expect("a successful render pins the commit it consumed");
    let (again, cursor_2) = render_once(&raw, root, Some(&cursor));
    assert_eq!(cursor_2.as_deref(), Some(cursor.as_str()));
    assert_eq!(again.len(), 1);
    let texts = |d: &RenderedMarkdown| {
        d.rows
            .iter()
            .map(|r| (r.uuid.clone(), r.preview.clone()))
            .collect::<Vec<_>>()
    };
    assert_eq!(texts(&again[0]), texts(doc));
    assert_eq!(
        again[0].bucket_key.as_deref(),
        Some(document_uuid(SOURCE).as_str())
    );
}
