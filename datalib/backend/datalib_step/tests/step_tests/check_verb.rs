//! `datalib-step run … --check` plans a step's params the way a sync
//! would and runs nothing: what `datalib-pipeline check` and the app's
//! save send. A misspelled param is refused by name; a good one passes.

use std::path::Path;
use std::process::{Command, Stdio};

fn check(root: &Path, params: &str) -> std::process::Output {
    Command::new(std::env::var_os("DATALIB_STEP_BIN").expect("DATALIB_STEP_BIN"))
        .args(["run", "ingest", "slack", "--check"])
        .env("DATALIB_DAG_STEP", "slack/ingest")
        .env("DATALIB_DAG_DATA_ROOT", root)
        .env("DATALIB_DAG_PARAMS", params)
        .stdin(Stdio::null())
        .output()
        .expect("run datalib-step --check")
}

#[test]
fn check_refuses_a_misspelled_param_by_name() {
    let td = tempfile::tempdir().unwrap();

    let out = check(td.path(), r#"{"api":{"chanels":["bridge"]}}"#);
    assert!(!out.status.success(), "{out:?}");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("chanels"),
        "{out:?}"
    );

    let out = check(td.path(), r#"{"api":{"channels":["bridge"]}}"#);
    assert!(out.status.success(), "{out:?}");
    assert!(
        std::fs::read_dir(td.path()).unwrap().next().is_none(),
        "a check writes nothing under the data root"
    );
}
