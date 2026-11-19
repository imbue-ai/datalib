//! `datalib-step check-params` reads a config's datalib-step steps' params
//! the way a sync would, which the runner cannot, and names a misspelled
//! one against its step before any sync runs. The app runs it on save.

use std::path::Path;
use std::process::Command;

use serde_json::Value;

fn check(dir: &Path, channels_key: &str) -> std::process::Output {
    std::fs::write(
        dir.join("config.toml"),
        format!(
            "[[groups]]\nid = \"slack\"\ntype = \"slack\"\n\n[[steps]]\nid = \"slack/ingest\"\n\
             command = \"datalib-step run ingest slack\"\n\n[steps.params.api]\n\
             {channels_key} = [\"bridge\"]\n"
        ),
    )
    .unwrap();
    Command::new(std::env::var_os("DATALIB_STEP_BIN").expect("DATALIB_STEP_BIN"))
        .arg("check-params")
        .arg(dir.join("config.toml"))
        .output()
        .expect("run datalib-step check-params")
}

#[test]
fn check_params_names_a_misspelled_param_against_its_step() {
    let td = tempfile::tempdir().unwrap();

    let out = check(td.path(), "chanels");
    assert!(out.status.success(), "{out:?}");
    let found: Vec<Value> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(|l| serde_json::from_str(l).expect("one JSON object per line"))
        .collect();
    assert_eq!(found.len(), 1, "{found:?}");
    assert_eq!(found[0]["step"], "slack/ingest");
    assert!(
        found[0]["message"].as_str().unwrap().contains("chanels"),
        "{found:?}"
    );

    let out = check(td.path(), "channels");
    assert!(out.status.success(), "{out:?}");
    assert!(out.stdout.is_empty(), "{out:?}");
}
