//! SIGINT to a running `datalib-step` ingest ends it at the next unit
//! boundary with a final commit — the last seal — and a `cancelled`
//! outcome. The real binary runs a slack playback slowed by
//! `DATALIB_HTTP_PLAYBACK_DELAY_MS`; the signal is sent when its stdout
//! reports the first channel done, so nothing here waits on the clock.
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use datalib_etl::synthesize::Synthesizer;
use datalib_etl_slack::synthesize::{write_recorded_call, SlackSynth};
use serde_json::{json, Value};

const TS_SINCE: &str = "1704067200.000000";
const STEP: &str = "work-slack/ingest";

fn write_fixture(api: &Path, channels: &[&str]) {
    let call = |method: &str, file: &str, params: Value, response: Value| {
        write_recorded_call(api, method, file, params, response).unwrap();
    };
    call(
        "auth.test",
        "run-1",
        json!({}),
        json!({"ok": true, "user_id": "U1", "team": "Enterprise", "team_id": "T1"}),
    );
    call(
        "users.list",
        "run-1",
        json!({"limit": "200"}),
        json!({"ok": true, "members": [
            {"id": "U1", "name": "picard", "real_name": "Jean-Luc Picard"},
        ]}),
    );
    let listed: Vec<Value> = channels
        .iter()
        .map(
            |c| json!({"id": c, "name": c.to_lowercase(), "is_member": true, "is_archived": false}),
        )
        .collect();
    call(
        "conversations.list",
        "run-1",
        json!({
            "exclude_archived": "true",
            "limit": "200",
            "types": "public_channel,private_channel",
        }),
        json!({"ok": true, "channels": listed, "has_more": false}),
    );
    for (i, c) in channels.iter().enumerate() {
        call(
            "conversations.history",
            c,
            json!({
                "channel": c,
                "include_all_metadata": "true",
                "inclusive": "true",
                "limit": "200",
                "oldest": TS_SINCE,
            }),
            json!({
                "ok": true,
                "messages": [{"ts": format!("1735689600.0001{i:02}"), "user": "U1", "text": format!("in {c}")}],
                "has_more": false,
            }),
        );
    }
}

fn doltlite(db: &Path, sql: &str) -> Vec<String> {
    let bin = std::env::var_os("DOLTLITE_BIN").expect("DOLTLITE_BIN");
    let out = Command::new(bin)
        .arg("-readonly")
        .arg(db)
        .arg(sql)
        .output()
        .expect("run doltlite");
    assert!(
        out.status.success(),
        "doltlite failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn sigint_ends_the_ingest_at_a_channel_boundary_with_a_final_commit() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("root");
    let api = d.path().join("input_raw");
    let playback = d.path().join("playback");
    fs::create_dir_all(&root).unwrap();
    write_fixture(&api, &["C1", "C2", "C3", "C4", "C5"]);
    SlackSynth::new(&api).synthesize(&playback).unwrap();
    let params = d.path().join("params.json");
    fs::write(
        &params,
        json!({"api": {"media": false, "dms": false}}).to_string(),
    )
    .unwrap();

    let bin = std::env::var_os("DATALIB_STEP_BIN").expect("DATALIB_STEP_BIN");
    let mut child = Command::new(bin)
        .arg("--params-file")
        .arg(&params)
        .env("DATALIB_DAG_STEP", STEP)
        .env("DATALIB_DAG_GROUP", "work-slack")
        .env("DATALIB_DAG_GROUP_TYPE", "slack")
        .env("DATALIB_DAG_FUNCTION", "ingest")
        .env("DATALIB_DAG_DATA_ROOT", &root)
        .env("DATALIB_DAG_NOW", "2026-09-21T00:00:00+00:00")
        .env("DATALIB_HTTP_PLAYBACK", &playback)
        // Every replayed request takes this long, so each channel is a
        // distinct stretch of time and the signal lands between two.
        .env("DATALIB_HTTP_PLAYBACK_DELAY_MS", "400")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("spawn datalib-step");
    let pid = child.id();

    // The step's own account of its progress: one `progress_inc` on the
    // step itself per channel finished. The first one is the moment.
    let stdout = child.stdout.take().unwrap();
    let reader = std::thread::spawn(move || {
        let mut lines = Vec::new();
        let mut signalled = false;
        for line in BufReader::new(stdout).lines() {
            let line = line.unwrap();
            if !signalled {
                if let Ok(v) = serde_json::from_str::<Value>(&line) {
                    if v["event"] == "progress_inc" && v["step"] == STEP {
                        // Safety: our own child, still running (we hold
                        // its handle), a valid signal.
                        unsafe { libc::kill(pid as libc::pid_t, libc::SIGINT) };
                        signalled = true;
                    }
                }
            }
            lines.push(line);
        }
        (lines, signalled)
    });

    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "datalib-step did not exit within 60s of the interrupt"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let (lines, signalled) = reader.join().unwrap();
    assert!(
        signalled,
        "the step never reported a finished channel:\n{}",
        lines.join("\n")
    );
    assert_eq!(
        status.code(),
        Some(130),
        "exit code; stdout:\n{}",
        lines.join("\n")
    );

    let outcome = lines
        .iter()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["event"] == "outcome")
        .expect("an outcome line");
    assert_eq!(outcome["failure"], "cancelled", "{outcome}");

    // The store is at a commit the run made on its way out, not at the
    // working set the signal found: `finish` ran, and what the walk had
    // done stands. The interrupt landed after channel 1 and before the
    // loop reached channel 3 at the latest; nothing after the boundary
    // was started.
    let db = root.join("work-slack/ingest/entities.doltlite_db");
    let messages = doltlite(
        &db,
        "SELECT message FROM dolt_log() ORDER BY date DESC LIMIT 1",
    );
    assert!(
        messages[0].starts_with("download work-slack:"),
        "the last commit is the run's own final commit, got {messages:?}"
    );
    let dirty = doltlite(&db, "SELECT count(*) FROM dolt_status");
    assert_eq!(dirty, vec!["0"], "everything the run kept is committed");
    let channels = doltlite(
        &db,
        "SELECT count(DISTINCT channel_id) FROM dolt_at_messages('HEAD')",
    );
    let n: usize = channels[0].parse().unwrap();
    assert!(
        (1..=2).contains(&n),
        "one channel finished before the signal and at most one was in flight; got {n}"
    );
}
