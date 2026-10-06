//! A source that renders no documents counts its items from one raw
//! table; the real binary's render step, over an fsindex store.
use std::process::{Command, Stdio};

use serde_json::Value;

/// fsindex renders nothing of its own, so its Items cell comes from
/// the `files` table the storage report counts. Without that, the
/// render step reported the store's documents' items — zero, however
/// many files it holds.
#[tokio::test]
async fn an_fsindex_render_reports_its_files_as_items() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    let store = datalib_etl::raw_layout::entities_db(&root.join("tng/ingest"));
    std::fs::create_dir_all(store.parent().unwrap()).unwrap();
    let pool = datalib_etl::doltlite_raw::open(
        &store,
        &["CREATE TABLE IF NOT EXISTS files (path TEXT PRIMARY KEY)"],
    )
    .await
    .unwrap();
    for path in ["bridge/log.txt", "bridge/map.png", "engineering/warp.dat"] {
        sqlx::query("INSERT INTO files (path) VALUES (?)")
            .bind(path)
            .execute(&pool)
            .await
            .unwrap();
    }
    datalib_etl::doltlite_raw::commit_run(&pool, "seed")
        .await
        .unwrap();
    pool.close().await;

    let out = Command::new(std::env::var_os("DATALIB_STEP_BIN").expect("DATALIB_STEP_BIN"))
        .env("DATALIB_DAG_STEP", "tng/render_markdown")
        .env("DATALIB_DAG_GROUP", "tng")
        .env("DATALIB_DAG_GROUP_TYPE", "fsindex")
        .env("DATALIB_DAG_FUNCTION", "render_markdown")
        .env("DATALIB_DAG_INPUTS", "tng/ingest")
        .env("DATALIB_DAG_DATA_ROOT", root)
        .stdin(Stdio::null())
        .output()
        .expect("spawn datalib-step");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let last = |name: &str| {
        stdout
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|v| v["event"] == "metric" && v["name"] == name)
            .filter_map(|v| v["value"].as_i64())
            .next_back()
    };
    assert_eq!(last("items"), Some(3), "{stdout}");
    assert_eq!(last("documents"), Some(0), "{stdout}");
}
