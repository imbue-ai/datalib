//! A store seeded from a data export, then kept fresh by the API. The
//! export fixture (`tests/fixtures/chatgpt_export`) is the API fixture's
//! account in the shape a real export has, read here as the `.zip`
//! ChatGPT sends.

use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_chatgpt::ingest::db::block_on_load_all;
use datalib_etl_chatgpt::ingest::export::{self, IngestOptions, IngestSummary};
use datalib_etl_chatgpt::ingest::{db_path_for, fetch, FetchOptions, RawDb};
use datalib_etl_chatgpt::synthesize::ChatgptSynth;
use datalib_etl_chatgpt_render::render::parse::parse_loaded;
use datalib_etl_chatgpt_render::render::render::render_all;
use datalib_etl_web::playback;
use datalib_etl_web::synthesize::Synthesizer;
use serde_json::Value;

fn fixture(var: &str, rel: &str) -> PathBuf {
    if let Ok(d) = std::env::var(var) {
        return PathBuf::from(d);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)
}

fn api_fixture() -> PathBuf {
    fixture("CHATGPT_FIXTURE_DIR", "tests/fixtures/chatgpt_api")
}

fn export_fixture() -> PathBuf {
    fixture(
        "CHATGPT_EXPORT_FIXTURE_DIR",
        "tests/fixtures/chatgpt_export",
    )
}

fn read_json(p: &Path) -> Value {
    serde_json::from_slice(&fs::read(p).unwrap()).unwrap()
}

fn write_json(p: &Path, v: &Value) {
    fs::create_dir_all(p.parent().unwrap()).unwrap();
    fs::write(p, serde_json::to_vec_pretty(v).unwrap()).unwrap();
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let p = e.unwrap().path();
        let dest = to.join(p.file_name().unwrap());
        if p.is_dir() {
            copy_dir(&p, &dest);
        } else {
            fs::copy(&p, &dest).unwrap();
        }
    }
}

/// `dir` zipped as ChatGPT ships an export: every file at the top of
/// the zip, `sites/` its one folder.
fn zip_of(dir: &Path, zip: &Path) {
    fn add(w: &mut zip::ZipWriter<File>, dir: &Path, root: &Path) {
        let mut entries: Vec<PathBuf> = fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .collect();
        entries.sort();
        for p in entries {
            if p.is_dir() {
                add(w, &p, root);
                continue;
            }
            let name = p.strip_prefix(root).unwrap().to_str().unwrap();
            w.start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            w.write_all(&fs::read(&p).unwrap()).unwrap();
        }
    }
    let mut w = zip::ZipWriter::new(File::create(zip).unwrap());
    add(&mut w, dir, dir);
    w.finish().unwrap();
}

async fn ingest_export(db: &RawDb, input_path: &Path) -> IngestSummary {
    let s = export::ingest(IngestOptions {
        db: db.clone(),
        input_path: input_path.to_path_buf(),
        now: "2026-10-22T00:00:00-07:00".into(),
        progress: Default::default(),
        control: Default::default(),
    })
    .await
    .unwrap();
    db.commit_all("export").await.unwrap();
    s
}

/// Every markdown file render writes from the store at `raw`.
fn rendered(raw: &Path) -> BTreeMap<String, String> {
    let parsed = parse_loaded(block_on_load_all(&db_path_for(raw)).unwrap());
    let out = tempfile::tempdir().unwrap();
    render_all(
        &parsed,
        out.path(),
        "chatgpt",
        &datalib_etl::progress::Progress::noop(),
        &mut |_| Ok(()),
        1,
    )
    .unwrap();
    let mut md = BTreeMap::new();
    let mut todo = vec![out.path().to_path_buf()];
    while let Some(d) = todo.pop() {
        for e in fs::read_dir(&d).unwrap() {
            let p = e.unwrap().path();
            if p.is_dir() {
                todo.push(p);
            } else if p.extension().is_some_and(|x| x == "md") {
                let rel = p.strip_prefix(out.path()).unwrap().display().to_string();
                md.insert(rel, fs::read_to_string(&p).unwrap());
            }
        }
    }
    md
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_api_run_after_an_export_fetches_only_what_moved() {
    let d = tempfile::tempdir().unwrap();
    let (api, zip, tape, raw) = (
        d.path().join("api"),
        d.path().join("chatgpt-export.zip"),
        d.path().join("playback"),
        d.path().join("raw"),
    );
    copy_dir(&api_fixture(), &api);
    zip_of(&export_fixture(), &zip);

    let db = RawDb::open(&db_path_for(&raw)).await.unwrap();
    let seeded = ingest_export(&db, &zip).await;
    assert_eq!(seeded.conversations, 5);
    // The generated image is in the export, but the tool message that
    // showed it is not, so nothing names it.
    assert_eq!((seeded.new_blobs, seeded.not_in_export), (3, 0));

    // After the export was taken, one conversation moved on chatgpt.com.
    let moved = "68fa0003-fake-7000-8000-positronic0003";
    let mut listing = read_json(&api.join("conversations.json"));
    for item in listing.as_array_mut().unwrap() {
        if item["id"] == moved {
            item["update_time"] = (item["update_time"].as_f64().unwrap() + 600.0).into();
        }
    }
    write_json(&api.join("conversations.json"), &listing);
    let detail_path = api.join(format!("conversations/{moved}.json"));
    let mut detail = read_json(&detail_path);
    detail["update_time"] = (detail["update_time"].as_f64().unwrap() + 600.0).into();
    write_json(&detail_path, &detail);
    ChatgptSynth::new(&api).synthesize(&tape).unwrap();

    let s = playback::scope(&tape, fetch(FetchOptions::new(db.clone())))
        .await
        .unwrap();
    db.commit_all("api").await.unwrap();
    db.close().await;
    assert_eq!((s.fetched, s.skipped, s.pruned), (1, 4, 0));
    assert_eq!(
        (s.new_blobs, s.failed_blobs),
        (0, 0),
        "every attachment came from the export"
    );
    // `/me`, the one listing page, the end of the listing, the one detail.
    assert!(s.requests <= 4, "{} requests", s.requests);
}

/// Each message section a rendered page holds, by its uuid, with the
/// author it is headed with.
fn sections(page: &str) -> Vec<(String, String)> {
    page.split("data-section-uuid=\"")
        .skip(1)
        .map(|rest| {
            let uuid = rest.split('"').next().unwrap().to_string();
            let author = rest
                .split("msg-author\">")
                .nth(1)
                .and_then(|a| a.split('<').next())
                .unwrap_or_default()
                .to_string();
            (uuid, author)
        })
        .collect()
}

/// An export leaves out the API's tool and system messages and every
/// node's `children`. Render reads no `children`, so a page from an
/// export holds every message a page from the API holds but its tool
/// steps and system prompt.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_export_renders_every_message_but_tool_and_system_ones() {
    let d = tempfile::tempdir().unwrap();
    let (zip, tape, from_api, from_export) = (
        d.path().join("chatgpt-export.zip"),
        d.path().join("playback"),
        d.path().join("from_api"),
        d.path().join("from_export"),
    );
    zip_of(&export_fixture(), &zip);
    ChatgptSynth::new(api_fixture()).synthesize(&tape).unwrap();

    let db = RawDb::open(&db_path_for(&from_api)).await.unwrap();
    playback::scope(&tape, fetch(FetchOptions::new(db.clone())))
        .await
        .unwrap();
    db.commit_all("api").await.unwrap();
    db.close().await;
    let db = RawDb::open(&db_path_for(&from_export)).await.unwrap();
    ingest_export(&db, &zip).await;
    db.close().await;

    let (api_md, export_md) = (rendered(&from_api), rendered(&from_export));
    assert_eq!(api_md.len(), 5);
    assert_eq!(
        api_md.keys().collect::<Vec<_>>(),
        export_md.keys().collect::<Vec<_>>()
    );
    let mut left_out = 0;
    for (path, page) in &api_md {
        let (from_api, from_export) = (sections(page), sections(&export_md[path]));
        let but_tools: Vec<_> = from_api
            .iter()
            .filter(|(_, author)| author != "Tool" && author != "System")
            .cloned()
            .collect();
        left_out += from_api.len() - but_tools.len();
        assert_eq!(from_export, but_tools, "{path}");
    }
    assert_eq!(
        left_out, 4,
        "the fixture's system prompt and tool steps: code output, a file read, an image"
    );
}
