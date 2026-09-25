//! The `embedding_map` function end to end, the real binary over the qmd
//! index the TNG fixture builds: a first run lays every embedded
//! document out afresh, a second starts from the first, and a reset
//! deletes the map so the next run starts over.
use std::path::Path;
use std::process::{Command, Stdio};

use serde_json::Value;

const STEP: &str = "unified_index/embedding_map";

fn materialize(root: &Path) {
    let tar = std::env::var_os("QMD_INDEX_TAR").expect("QMD_INDEX_TAR");
    std::fs::create_dir_all(root).unwrap();
    let status = Command::new("tar")
        .arg("-xf")
        .arg(tar)
        .arg("-C")
        .arg(root)
        .arg("--strip-components=1")
        .status()
        .expect("spawn tar");
    assert!(status.success(), "extracting the qmd index: {status}");
}

/// Run the step and return its outcome line.
fn run(root: &Path, reset: bool) -> Value {
    let bin = std::env::var_os("DATALIB_STEP_BIN").expect("DATALIB_STEP_BIN");
    let mut cmd = Command::new(bin);
    cmd.env("DATALIB_DAG_STEP", STEP)
        .env("DATALIB_DAG_GROUP", "unified_index")
        .env("DATALIB_DAG_FUNCTION", "embedding_map")
        .env("DATALIB_DAG_INPUTS", r#"["unified_index/qmd_index"]"#)
        .env("DATALIB_DAG_DATA_ROOT", root)
        .env("DATALIB_DAG_NOW", "2026-09-25T00:00:00+00:00")
        .stdin(Stdio::null());
    if reset {
        cmd.env("DATALIB_DAG_RESET", "store");
    }
    let out = cmd.output().expect("spawn datalib-step");
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    assert!(
        out.status.success(),
        "datalib-step failed: {}\n{stdout}\n{}",
        out.status,
        String::from_utf8_lossy(&out.stderr)
    );
    stdout
        .lines()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["event"] == "outcome")
        .unwrap_or_else(|| panic!("no outcome line in:\n{stdout}"))
}

fn map(root: &Path) -> Option<Value> {
    let path = root.join("unified_index/embedding_map/embedding_map.json");
    std::fs::read(path)
        .ok()
        .map(|b| serde_json::from_slice(&b).expect("the map parses"))
}

#[test]
fn a_map_is_laid_out_then_kept_then_reset() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("root");
    materialize(&root);

    let outcome = run(&root, false);
    assert_eq!(outcome["outputs"][0]["path"], STEP);
    assert!(outcome["outputs"][0]["version"]
        .as_str()
        .is_some_and(|v| v.starts_with("blake3:")));
    let first = map(&root).expect("a map after the first run");
    let n = first["points"].as_array().unwrap().len();
    assert!(n > 20, "the fixture embeds a few dozen documents, got {n}");
    assert_eq!(first["seed"]["fresh"], n);
    assert_eq!(first["seed"]["kept"], 0);
    assert_eq!(first["unembedded"], 0);
    for p in first["points"].as_array().unwrap() {
        assert!(
            p["path"].as_str().unwrap().contains("/render_markdown/"),
            "{p}"
        );
        assert!(p["x"].as_f64().unwrap().is_finite());
    }

    run(&root, false);
    let second = map(&root).expect("a map after the second run");
    assert_eq!(
        second["seed"]["kept"], n,
        "the second run starts from the first"
    );
    assert_eq!(second["seed"]["fresh"], 0);

    let outcome = run(&root, true);
    assert_eq!(outcome["outputs"], Value::Array(Vec::new()));
    assert!(map(&root).is_none(), "a reset deletes the map");
    run(&root, false);
    assert_eq!(
        map(&root).unwrap()["seed"]["fresh"],
        n,
        "and the next run starts over"
    );
}
