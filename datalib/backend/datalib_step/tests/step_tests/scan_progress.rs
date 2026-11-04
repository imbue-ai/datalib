//! What a real step says while it scans a folder: the messages a Manage row
//! shows, and the log lines, against the real binary.
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde_json::Value;

struct Ran {
    messages: Vec<String>,
    log: Vec<String>,
}

fn ingest(root: &Path, group_type: &str, params: Value) -> Ran {
    let file = root.join("params.json");
    std::fs::write(&file, params.to_string()).unwrap();
    let out = Command::new(std::env::var_os("DATALIB_STEP_BIN").expect("DATALIB_STEP_BIN"))
        .arg("--params-file")
        .arg(&file)
        .env("DATALIB_DAG_STEP", "source/ingest")
        .env("DATALIB_DAG_GROUP", "source")
        .env("DATALIB_DAG_GROUP_TYPE", group_type)
        .env("DATALIB_DAG_FUNCTION", "ingest")
        .env("DATALIB_DAG_DATA_ROOT", root)
        .env("DATALIB_CACHE_DIR", root.join("cache"))
        .stdin(Stdio::null())
        .output()
        .expect("spawn datalib-step");
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "{stderr}");
    let json = |text: &str| -> Vec<Value> {
        text.lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .collect()
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    Ran {
        messages: json(&stdout)
            .iter()
            .filter(|v| v["event"] == "progress_message")
            .filter_map(|v| v["msg"].as_str().map(str::to_string))
            .collect(),
        log: json(&stderr)
            .iter()
            .filter_map(|v| v["fields"]["message"].as_str().map(str::to_string))
            .collect(),
    }
}

fn has(lines: &[String], start: &str) -> bool {
    lines.iter().any(|l| l.starts_with(start))
}

/// A source that hashes first: a step's row said nothing while it did, and
/// only the log knew.
#[test]
fn a_step_that_hashes_first_says_so_on_its_row_and_in_its_log() {
    let d = tempfile::tempdir().unwrap();
    let fixture = PathBuf::from(std::env::var_os("SMS_FIXTURE_DIR").expect("SMS_FIXTURE_DIR"));
    let ran = ingest(
        d.path(),
        "sms_backup_restore",
        serde_json::json!({ "backup": { "path": fixture } }),
    );

    assert!(has(&ran.messages, "listing "), "{:?}", ran.messages);
    assert!(has(&ran.messages, "hashing "), "{:?}", ran.messages);
    assert!(has(&ran.log, "file_changes_scan_listed: "), "{:?}", ran.log);
    assert!(has(&ran.log, "file_changes_scan_hashed: "), "{:?}", ran.log);
}

/// A source that reads whole files: its log went quiet when the hashing moved
/// into its reads, so the reads say what they did.
#[test]
fn a_step_that_hashes_as_it_reads_says_so_in_its_log() {
    let d = tempfile::tempdir().unwrap();
    let fixture = PathBuf::from(
        std::env::var_os("CALENDAR_SPLIT_FIXTURE_DIR").expect("CALENDAR_SPLIT_FIXTURE_DIR"),
    )
    .join("before");
    let ran = ingest(
        d.path(),
        "calendar",
        serde_json::json!({ "ics": { "path": fixture } }),
    );

    assert!(has(&ran.messages, "listing "), "{:?}", ran.messages);
    assert!(!has(&ran.messages, "hashing "), "{:?}", ran.messages);
    assert!(has(&ran.log, "file_changes_scan_listed: "), "{:?}", ran.log);
    assert!(has(&ran.log, "file_changes_scan_read: "), "{:?}", ran.log);
}
