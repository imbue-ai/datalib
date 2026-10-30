//! `datalib-step probe`, run by hand, against a slack playback.
use std::path::Path;
use std::process::{Command, Output};

use datalib_etl_slack::recorded::record_workspace;
use datalib_etl_slack::synthesize::SlackSynth;
use datalib_etl_web::synthesize::Synthesizer;
use serde_json::{json, Value};

fn probe_slack(playback: &Path, rust_log: Option<&str>) -> Output {
    let mut cmd = Command::new(std::env::var_os("DATALIB_STEP_BIN").expect("DATALIB_STEP_BIN"));
    cmd.args(["probe", "slack"])
        .env("DATALIB_HTTP_PLAYBACK", playback)
        .env_remove("RUST_LOG");
    if let Some(filter) = rust_log {
        cmd.env("RUST_LOG", filter);
    }
    cmd.output().expect("run datalib-step probe")
}

/// The probe printed a DEBUG `item_fetched` line on stderr before its
/// answer (#1426): it took the step's `trace` default, meant for lines
/// the runner stores. By hand it says on stderr only what went wrong,
/// unless `RUST_LOG` asks for more.
#[test]
fn a_probe_that_succeeds_is_silent_on_stderr_unless_asked() {
    let d = tempfile::tempdir().unwrap();
    let api = d.path().join("input_raw");
    let playback = d.path().join("playback");
    record_workspace(&api, &["C1"], |_, _| json!([])).unwrap();
    SlackSynth::new(&api).synthesize(&playback).unwrap();

    let info = probe_slack(&playback, Some("info"));
    assert!(info.status.success(), "{info:?}");
    assert_eq!(String::from_utf8_lossy(&info.stderr), "");

    let quiet = probe_slack(&playback, None);
    assert!(quiet.status.success(), "{quiet:?}");
    let report: Value = serde_json::from_slice(&quiet.stdout).expect("one JSON object");
    assert_eq!(report["account"]["id"], "U1", "{report}");
    assert_eq!(String::from_utf8_lossy(&quiet.stderr), "");

    let asked = probe_slack(&playback, Some("datalib=debug"));
    assert!(asked.status.success(), "{asked:?}");
    assert!(
        String::from_utf8_lossy(&asked.stderr).contains("item_fetched"),
        "{asked:?}"
    );
}
