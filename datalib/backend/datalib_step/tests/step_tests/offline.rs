//! A network that goes away mid-sync ends the ingest `incomplete`, with
//! what it fetched committed. Each remaining Slack channel used to fail on
//! its first request (curl exit 6 was not retried) and the run ended
//! `succeeded`. The real binary runs a slack playback that goes offline
//! once its stdout reports the first channel done.
use std::fs;
use std::io::{BufRead, BufReader};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use datalib_etl_slack::recorded::record_workspace;
use datalib_etl_slack::synthesize::SlackSynth;
use datalib_etl_web::synthesize::Synthesizer;
use serde_json::{json, Value};

use crate::interrupt::doltlite;

const STEP: &str = "work-slack/ingest";
const CHANNELS: [&str; 5] = ["C1", "C2", "C3", "C4", "C5"];

#[test]
fn a_network_that_goes_away_ends_the_ingest_incomplete_with_what_it_got() {
    let d = tempfile::tempdir().unwrap();
    let root = d.path().join("root");
    let api = d.path().join("input_raw");
    let playback = d.path().join("playback");
    let offline = d.path().join("offline");
    fs::create_dir_all(&root).unwrap();
    record_workspace(&api, &CHANNELS, |i, c| {
        json!([{"ts": format!("1735689600.0001{i:02}"), "user": "U1", "text": format!("in {c}")}])
    })
    .unwrap();
    SlackSynth::new(&api).synthesize(&playback).unwrap();
    let params = d.path().join("params.json");
    fs::write(
        &params,
        json!({
            "api": {"media": false, "dms": false},
            // Two failures in a row is a give-up, so the test waits one
            // backoff rather than the default's half hour.
            "common": {"download_params": {"maximum_sequential_failed_requests": 2}},
        })
        .to_string(),
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
        .env("DATALIB_HTTP_PLAYBACK_OFFLINE", &offline)
        // Each channel takes a while, so the network goes between two.
        .env("DATALIB_HTTP_PLAYBACK_DELAY_MS", "400")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn datalib-step");

    let stdout = child.stdout.take().unwrap();
    let went_offline = offline.clone();
    let reader = std::thread::spawn(move || {
        let mut lines = Vec::new();
        for line in BufReader::new(stdout).lines() {
            let line = line.unwrap();
            if let Ok(v) = serde_json::from_str::<Value>(&line) {
                if v["event"] == "progress_inc" && v["step"] == STEP && !went_offline.exists() {
                    fs::write(&went_offline, "").unwrap();
                }
            }
            lines.push(line);
        }
        lines
    });
    let stderr = child.stderr.take().unwrap();
    let errors = std::thread::spawn(move || {
        BufReader::new(stderr)
            .lines()
            .map_while(Result::ok)
            .collect::<Vec<_>>()
    });

    let deadline = Instant::now() + Duration::from_secs(60);
    let status = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break status;
        }
        assert!(
            Instant::now() < deadline,
            "datalib-step did not end within 60s of going offline"
        );
        std::thread::sleep(Duration::from_millis(50));
    };
    let lines = reader.join().unwrap();
    let stderr = errors.join().unwrap().join("\n");
    assert!(
        offline.exists(),
        "the step never reported a finished channel"
    );
    assert!(!status.success(), "a give-up is not a success:\n{stderr}");
    assert_ne!(status.code(), Some(130), "nor a stop someone asked for");

    let outcome = lines
        .iter()
        .rev()
        .filter_map(|l| serde_json::from_str::<Value>(l).ok())
        .find(|v| v["event"] == "outcome")
        .expect("an outcome line");
    assert_eq!(outcome["failure"], "incomplete", "{outcome}");
    assert_eq!(
        outcome["outputs"].as_array().map(Vec::len),
        Some(1),
        "the version of what it committed reaches its consumers: {outcome}"
    );
    assert!(
        stderr.contains("gave up on slack") && stderr.contains("Could not resolve host"),
        "the step says why it stopped:\n{stderr}"
    );
    assert_eq!(
        crate::interrupt::doltlite(
            &root.join("work-slack/ingest/entities.doltlite_db"),
            "SELECT status FROM sync_runs ORDER BY run_id DESC LIMIT 1",
        ),
        vec!["partial"],
        "a run that gave up did not read everything, so its row must not say `ok`"
    );

    // What it fetched before the network went is committed; the rest is
    // owed to the next run, not walked through one failure at a time.
    let db = root.join("work-slack/ingest/entities.doltlite_db");
    let last = doltlite(
        &db,
        "SELECT message FROM dolt_log() ORDER BY date DESC LIMIT 1",
    );
    assert!(last[0].starts_with("download work-slack:"), "{last:?}");
    assert_eq!(doltlite(&db, "SELECT count(*) FROM dolt_status"), ["0"]);
    let channels: usize = doltlite(
        &db,
        "SELECT count(DISTINCT channel_id) FROM dolt_at_messages('HEAD')",
    )[0]
    .parse()
    .unwrap();
    assert!(
        (1..CHANNELS.len()).contains(&channels),
        "the first channel landed and the rest did not; got {channels}"
    );
}
