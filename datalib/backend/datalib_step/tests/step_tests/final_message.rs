//! What a download's Manage row says when it is done, against the real
//! binary over the SMS fixture.
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::Value;

/// A row read whatever the provider last said mid-walk: "msgs=0 replies=0
/// media=0" for a Slack sync that fetched ten thousand messages (the
/// last channel's counts), "fetching" for Gmail. The row's last word is
/// the download's own summary.
#[tokio::test]
async fn a_download_ends_on_its_summary() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path();
    let fixture = PathBuf::from(std::env::var_os("SMS_FIXTURE_DIR").expect("SMS_FIXTURE_DIR"));
    let params = root.join("params.json");
    std::fs::write(
        &params,
        serde_json::json!({ "backup": { "path": fixture } }).to_string(),
    )
    .unwrap();
    let out = Command::new(std::env::var_os("DATALIB_STEP_BIN").expect("DATALIB_STEP_BIN"))
        .arg("--params-file")
        .arg(&params)
        .env("DATALIB_DAG_STEP", "phone/ingest")
        .env("DATALIB_DAG_GROUP", "phone")
        .env("DATALIB_DAG_GROUP_TYPE", "sms_backup_restore")
        .env("DATALIB_DAG_FUNCTION", "ingest")
        .env("DATALIB_DAG_DATA_ROOT", root)
        .env("DATALIB_CACHE_DIR", root.join("cache"))
        .stdin(Stdio::null())
        .output()
        .expect("spawn datalib-step");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");

    let json_lines = |text: &str| -> Vec<Value> {
        text.lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .collect()
    };
    let summary = json_lines(&stderr)
        .into_iter()
        .find(|v| v["fields"]["message"] == "the download is done")
        .and_then(|v| v["fields"]["summary"].as_str().map(str::to_string))
        .unwrap_or_else(|| panic!("no summary line on stderr:\n{stderr}"));
    let last_message = json_lines(&stdout)
        .into_iter()
        .filter(|v| v["event"] == "progress_message")
        .filter_map(|v| v["msg"].as_str().map(str::to_string))
        .next_back();
    assert_eq!(last_message.as_deref(), Some(summary.as_str()), "{stdout}");
}
