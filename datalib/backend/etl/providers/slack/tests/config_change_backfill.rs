//! Two-run tests for the config-change adjustments.
//!
//! The unit tests in `download/mod.rs` cover `Adjustments::plan` as a
//! pure predicate. These cover the part that actually moves data: that a
//! widened `since` on run 2 issues the bounded backfill call and lands
//! the older messages, and that an unchanged config doesn't.
//!
//! Why this needs an integration test: every gate on the backfill path
//! fails *silently to a no-op*. If the `Adjustments` plumbing into
//! `export_channel` were dropped, `channel_ts_bounds.get(cid)` would
//! yield `None`, the `if let Some(oldest)` would skip the backfill, and
//! every unit test would still pass — reproducing the exact bug this
//! machinery exists to fix.
//!
//! Same synth → playback → download shape as `playback_roundtrip.rs`;
//! the two-run structure follows `chatgpt/tests/incremental_skip.rs`.

use std::fs;
use std::path::Path;

use frankweiler_etl::http::PLAYBACK_ENV;
use frankweiler_etl::synthesize::Synthesizer;
use frankweiler_etl_slack::download::{block_on_load_all, db_path_for, fetch, FetchOptions};
use frankweiler_etl_slack::synthesize::SlackSynth;
use serde_json::{json, Value};
use tempfile::tempdir;
use tokio::sync::Mutex;

/// `PLAYBACK_ENV` is process-global, so the scenarios below cannot run
/// concurrently — each would clobber the others' playback root. Held for
/// the whole body of each test (a `tokio` mutex, so it survives the
/// `.await`s).
static ENV_LOCK: Mutex<()> = Mutex::const_new(());

/// `datetime_to_slack_ts` of the corresponding UTC midnight — the exact
/// `oldest` param the downloader emits for each `since` value.
const TS_2023: &str = "1672531200.000000";
const TS_2024: &str = "1704067200.000000";

/// Messages the fixtures serve. `OLD` predates `since: 2024-01-01`, so
/// only a widened `since` can reach it.
const TS_OLD: &str = "1688000000.000000"; // 2023-06-29
const TS_NEW: &str = "1735689600.000000"; // 2025-01-01

fn write_envelope(path: &Path, line: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut s = serde_json::to_string(line).unwrap();
    s.push('\n');
    fs::write(path, s).unwrap();
}

/// Envelopes for auth/channels/users, shared by every run.
fn write_setup_fixtures(api: &Path) {
    write_envelope(
        &api.join("raw_api/auth.test/run-1.jsonl"),
        &json!({
            "method": "auth.test", "params": {},
            "response": {"ok": true, "user_id": "U1", "team": "Enterprise", "team_id": "T1"},
        }),
    );
    write_envelope(
        &api.join("raw_api/conversations.list/run-1.jsonl"),
        &json!({
            "method": "conversations.list",
            "params": {
                "exclude_archived": "true",
                "limit": "200",
                "types": "public_channel,private_channel",
            },
            "response": {
                "ok": true,
                "channels": [{"id": "C1", "name": "general", "is_member": true}],
            },
        }),
    );
    write_envelope(
        &api.join("raw_api/users.list/run-1.jsonl"),
        &json!({
            "method": "users.list",
            "params": {"limit": "200"},
            "response": {"ok": true, "members": [{"id": "U1", "name": "alice"}]},
        }),
    );
}

/// One `conversations.history` envelope. Playback keys on the exact
/// param set, so each distinct `(oldest, latest, inclusive)` triple the
/// downloader emits needs its own fixture — which is what makes this
/// test sensitive to the backfill call being made at all.
fn write_history(
    api: &Path,
    run: &str,
    oldest: &str,
    latest: Option<&str>,
    inclusive: bool,
    messages: Value,
) {
    let mut params = json!({
        "channel": "C1",
        "include_all_metadata": "true",
        "inclusive": if inclusive { "true" } else { "false" },
        "limit": "200",
        "oldest": oldest,
    });
    if let Some(l) = latest {
        params["latest"] = json!(l);
    }
    write_envelope(
        &api.join(format!("raw_api/conversations.history/{run}.jsonl")),
        &json!({
            "method": "conversations.history",
            "params": params,
            "response": {"ok": true, "messages": messages, "has_more": false},
        }),
    );
}

fn msg(ts: &str, text: &str) -> Value {
    json!({"ts": ts, "user": "U1", "text": text})
}

async fn run_fetch(out: &Path, since: &str) {
    fetch(FetchOptions {
        db_path: out.to_path_buf(),
        channels: None,
        since: since.into(),
        refresh_window_days: 0,
        members_only: false,
        media: false,
        ..Default::default()
    })
    .await
    .unwrap();
}

fn stored_ts(out: &Path) -> Vec<String> {
    let raw = block_on_load_all(&db_path_for(out)).expect("load db");
    let mut ts: Vec<String> = raw.messages.iter().map(|m| m.ts.clone()).collect();
    ts.sort();
    ts
}

/// The headline behavior: widening `since` on an already-synced store
/// fetches the newly-in-scope window below the oldest stored message.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn widened_since_backfills_below_oldest_stored_message() {
    let _guard = ENV_LOCK.lock().await;
    let d = tempdir().unwrap();
    let api = d.path().join("input_raw");
    let playback = d.path().join("playback");
    let out = d.path().join("out_raw");

    write_setup_fixtures(&api);
    // Run 1, `since: 2024-01-01` — cold start, so `oldest` is the
    // configured since and only the 2025 message is in scope.
    write_history(
        &api,
        "run-1",
        TS_2024,
        None,
        true,
        json!([msg(TS_NEW, "new")]),
    );
    // Run 2, `since: 2023-01-01`. Two calls are expected:
    //   - the forward walk, resuming at the stored watermark (exclusive)
    //   - the backfill, `[2023-01-01, oldest_stored]` inclusive
    write_history(&api, "run-2", TS_NEW, None, false, json!([]));
    write_history(
        &api,
        "run-3",
        TS_2023,
        Some(TS_NEW),
        true,
        json!([msg(TS_OLD, "old")]),
    );

    SlackSynth::new(&api).synthesize(&playback).unwrap();
    std::env::set_var(PLAYBACK_ENV, &playback);

    run_fetch(&out, "2024-01-01").await;
    assert_eq!(
        stored_ts(&out),
        vec![TS_NEW.to_string()],
        "run 1 should mirror only the in-scope message",
    );

    run_fetch(&out, "2023-01-01").await;
    assert_eq!(
        stored_ts(&out),
        vec![TS_OLD.to_string(), TS_NEW.to_string()],
        "run 2 widened `since`, so the older message must be backfilled",
    );
}

/// The steady state: re-running with the same config plans no work, so
/// no backfill call is issued. Guards against the adjustment firing on
/// every run (which would re-walk the whole archive each sync).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unchanged_since_issues_no_backfill() {
    let _guard = ENV_LOCK.lock().await;
    let d = tempdir().unwrap();
    let api = d.path().join("input_raw");
    let playback = d.path().join("playback");
    let out = d.path().join("out_raw");

    write_setup_fixtures(&api);
    write_history(
        &api,
        "run-1",
        TS_2024,
        None,
        true,
        json!([msg(TS_NEW, "new")]),
    );
    // Only the forward walk is served. A backfill call would 404 against
    // playback and fail the run — which is exactly the assertion.
    write_history(&api, "run-2", TS_NEW, None, false, json!([]));

    SlackSynth::new(&api).synthesize(&playback).unwrap();
    std::env::set_var(PLAYBACK_ENV, &playback);

    run_fetch(&out, "2024-01-01").await;
    run_fetch(&out, "2024-01-01").await;

    assert_eq!(stored_ts(&out), vec![TS_NEW.to_string()]);
}

/// Narrowing is a no-op: the store is already a superset, and nothing in
/// the pipeline deletes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn narrowed_since_keeps_existing_messages_and_issues_no_backfill() {
    let _guard = ENV_LOCK.lock().await;
    let d = tempdir().unwrap();
    let api = d.path().join("input_raw");
    let playback = d.path().join("playback");
    let out = d.path().join("out_raw");

    write_setup_fixtures(&api);
    write_history(
        &api,
        "run-1",
        TS_2023,
        None,
        true,
        json!([msg(TS_OLD, "old"), msg(TS_NEW, "new")]),
    );
    write_history(&api, "run-2", TS_NEW, None, false, json!([]));

    SlackSynth::new(&api).synthesize(&playback).unwrap();
    std::env::set_var(PLAYBACK_ENV, &playback);

    run_fetch(&out, "2023-01-01").await;
    run_fetch(&out, "2024-01-01").await;

    assert_eq!(
        stored_ts(&out),
        vec![TS_OLD.to_string(), TS_NEW.to_string()],
        "narrowing must not drop already-mirrored messages",
    );
}
