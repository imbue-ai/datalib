//! The reset verb on a download step, the real binary over a store with
//! one problem in it.
use std::collections::BTreeMap;
use std::process::{Command, Stdio};

use datalib_etl::download_problems::{report_run, RunProblem};
use serde_json::Value;

/// The Manage row shows a step's newest `problems` count, and the app's
/// Reset does not run the step again. A reset that reported no count
/// left the pre-reset one standing: 34 errors on an emptied store.
#[tokio::test]
async fn a_reset_download_reports_the_emptied_stores_problems() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    let store = datalib_etl::raw_layout::entities_db(&root.join("tng/ingest"));
    std::fs::create_dir_all(store.parent().unwrap()).unwrap();
    let pool = datalib_etl::doltlite_raw::open(&store, &[]).await.unwrap();
    report_run(
        &pool,
        &[RunProblem::phase("messages", "upstream fell over")],
    )
    .await;
    datalib_etl::doltlite_raw::commit_run(&pool, "seed")
        .await
        .unwrap();
    pool.close().await;

    let out = Command::new(std::env::var_os("DATALIB_STEP_BIN").expect("DATALIB_STEP_BIN"))
        .env("DATALIB_DAG_STEP", "tng/ingest")
        .env("DATALIB_DAG_GROUP", "tng")
        .env("DATALIB_DAG_GROUP_TYPE", "slack")
        .env("DATALIB_DAG_FUNCTION", "ingest")
        .env("DATALIB_DAG_DATA_ROOT", root)
        .env("DATALIB_DAG_RESET", "store")
        .stdin(Stdio::null())
        .output()
        .expect("spawn datalib-step");
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success(),
        "{stdout}\n{}",
        String::from_utf8_lossy(&out.stderr)
    );

    let counts: BTreeMap<String, i64> = stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .filter(|v| v["event"] == "metric" && v["name"] == "problems")
        .map(|v| {
            (
                v["labels"]["severity"].as_str().unwrap().to_string(),
                v["value"].as_i64().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        counts,
        BTreeMap::from([("error".into(), 0), ("warning".into(), 0)]),
        "{stdout}"
    );
}
