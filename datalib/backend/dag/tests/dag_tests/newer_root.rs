//! The runner refuses a data root a newer line of datalib wrote — on
//! `--check` and on a run alike — before it takes the runner lock,
//! reads its scheduler state, or starts a step. The step here writes a
//! marker file, so "the step never ran" is a file that must not exist.

use std::path::Path;
use std::process::Command;
use std::str::FromStr;

use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};

fn dag_bin() -> String {
    Path::new(&std::env::var("DATALIB_DAG_BIN").expect("DATALIB_DAG_BIN is set"))
        .canonicalize()
        .expect("canonical dag path")
        .to_string_lossy()
        .into_owned()
}

/// A raw store as a newer release would have left it: a `_datalib_meta`
/// naming a version from the future, and nothing else.
async fn store_written_by(path: &Path, version: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
        .unwrap()
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .unwrap();
    datalib_store_meta::write(&pool, datalib_store_meta::StoreKind::Raw, "h", 0)
        .await
        .unwrap();
    sqlx::query("UPDATE _datalib_meta SET value = ? WHERE key = 'datalib_version'")
        .bind(version)
        .execute(&pool)
        .await
        .unwrap();
    pool.close().await;
}

#[tokio::test]
async fn a_root_a_newer_build_wrote_is_refused_before_any_step_runs() {
    let td = tempfile::tempdir().unwrap();
    let root = td.path().canonicalize().unwrap();
    std::fs::write(
        root.join("config.toml"),
        "[[steps]]\nid = \"probe/step\"\n\
         command = \"sh -c 'mkdir -p probe/step; echo ran > probe/step/ran'\"\n",
    )
    .unwrap();
    store_written_by(&root.join("slack/ingest/entities.doltlite_db"), "99.0.0").await;

    for args in [vec!["--check"], vec![]] {
        let out = Command::new(dag_bin())
            .args(&args)
            .arg(root.join("config.toml"))
            .output()
            .expect("run datalib-dag");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!out.status.success(), "{args:?} must fail: {stderr}");
        assert!(
            stderr.contains("written by datalib 99.0.0"),
            "{args:?} names the version that wrote the store: {stderr}"
        );
        assert!(
            stderr.contains("slack/ingest/entities.doltlite_db"),
            "{args:?} names the store: {stderr}"
        );
    }
    assert!(
        !root.join("probe/step/ran").exists(),
        "the step must not have run"
    );
    assert!(
        !root.join("system/supervisor.sqlite").exists(),
        "the record must not have been written"
    );

    // The same root, once the store says the running line wrote it, runs.
    store_written_by(
        &root.join("slack/ingest/entities.doltlite_db"),
        datalib_runtime::build_id::DATALIB_VERSION,
    )
    .await;
    let out = Command::new(dag_bin())
        .arg(root.join("config.toml"))
        .output()
        .expect("run datalib-dag");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(root.join("probe/step/ran").exists());
}
