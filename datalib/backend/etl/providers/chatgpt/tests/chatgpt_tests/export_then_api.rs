//! A store seeded from a data export, then kept fresh by the API: the
//! API run fetches only what moved since the export, and no attachment.

use std::fs;
use std::path::{Path, PathBuf};

use datalib_etl::store_handle::RawStoreHandle;
use datalib_etl_chatgpt::ingest::export::{self, IngestOptions};
use datalib_etl_chatgpt::ingest::{db_path_for, fetch, FetchOptions, RawDb};
use datalib_etl_chatgpt::synthesize::ChatgptSynth;
use datalib_etl_web::playback;
use datalib_etl_web::synthesize::Synthesizer;
use serde_json::Value;

fn fixture_dir() -> PathBuf {
    if let Ok(d) = std::env::var("CHATGPT_FIXTURE_DIR") {
        return PathBuf::from(d);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/chatgpt_api")
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

/// The fixture's account laid out as a data export: every conversation
/// whole in one `conversations.json`, the account in `user.json`, and
/// each file named for its id and an upload name, in a folder.
fn export_of(api: &Path, export: &Path) {
    let mut convs = Vec::new();
    for item in read_json(&api.join("conversations.json"))
        .as_array()
        .unwrap()
    {
        let id = item["id"].as_str().unwrap();
        convs.push(read_json(&api.join(format!("conversations/{id}.json"))));
    }
    write_json(&export.join("conversations.json"), &Value::Array(convs));
    write_json(&export.join("user.json"), &read_json(&api.join("me.json")));
    let uploads = export.join("user-FAKE0DATA");
    fs::create_dir_all(&uploads).unwrap();
    for e in fs::read_dir(api.join("files")).unwrap() {
        let p = e.unwrap().path();
        let (stem, ext) = (
            p.file_stem().unwrap().to_str().unwrap(),
            p.extension().unwrap().to_str().unwrap(),
        );
        fs::copy(&p, uploads.join(format!("{stem}-holodeck-upload.{ext}"))).unwrap();
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_api_run_after_an_export_fetches_only_what_moved() {
    let d = tempfile::tempdir().unwrap();
    let (api, ex, tape, raw) = (
        d.path().join("api"),
        d.path().join("export"),
        d.path().join("playback"),
        d.path().join("raw"),
    );
    copy_dir(&fixture_dir(), &api);
    export_of(&api, &ex);

    let db = RawDb::open(&db_path_for(&raw)).await.unwrap();
    let seeded = export::ingest(IngestOptions {
        db: db.clone(),
        input_path: ex.clone(),
        now: "2026-10-22T00:00:00-07:00".into(),
        progress: Default::default(),
        control: Default::default(),
    })
    .await
    .unwrap();
    assert_eq!(seeded.conversations, 5);
    assert_eq!((seeded.new_blobs, seeded.not_in_export), (4, 0));
    db.commit_all("export").await.unwrap();

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
