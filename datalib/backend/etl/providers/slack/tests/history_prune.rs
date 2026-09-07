//! Slack has no tombstones and no changes cursor: a deleted message just
//! stops appearing in `conversations.history`. The only way to see that is
//! to re-walk a bounded range and compare, which the trailing
//! `refresh_window_days` pass already does. These are two-run playback
//! tests over that: run 1 mirrors a channel, run 2 serves a refresh window
//! that omits one message, and the question is what happens to our copy.

use std::fs;
use std::path::Path;

use datalib_etl::http::PLAYBACK_ENV;
use datalib_etl::synthesize::Synthesizer;
use datalib_etl_slack::download::{block_on_load_all, db_path_for, fetch, FetchOptions};
use datalib_etl_slack::synthesize::SlackSynth;
use serde_json::{json, Value};
use tempfile::tempdir;
use tokio::sync::Mutex;

/// `PLAYBACK_ENV` is process-global, so these cannot run concurrently.
static ENV_LOCK: Mutex<()> = Mutex::const_new(());

/// Far enough back that `refresh_window_days: 3650` covers every message
/// here, so the window pass re-walks the whole channel.
///
/// Deliberately in the 10-digit-epoch era. Slack timestamps are compared
/// as *strings* throughout this provider — in the window bounds and in the
/// `ts BETWEEN` the prune issues — so a `since` before 2001-09-09 sorts
/// above every real message ("946..." > "17...") and the window silently
/// matches nothing. Every genuine Slack ts is 10 digits, so this only ever
/// bites a fixture; it cost an afternoon here, hence the note.
const SINCE: &str = "2020-01-01";
const TS_A: &str = "1700000000.000000";
const TS_B: &str = "1700000100.000000";
const TS_C: &str = "1700000200.000000";

fn write_envelope(path: &Path, line: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut s = serde_json::to_string(line).unwrap();
    s.push('\n');
    fs::write(path, s).unwrap();
}

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

/// `datetime_to_slack_ts` of UTC midnight on `SINCE` — the `oldest` the
/// downloader sends on a cold start, and (because the refresh window
/// reaches further back than it) the window pass's `oldest` too.
fn since_ts() -> String {
    "1577836800.000000".to_string()
}

async fn run_fetch(out: &Path, refresh_window_days: i64) -> usize {
    let s = fetch(FetchOptions {
        db_path: out.to_path_buf(),
        channels: None,
        since: SINCE.into(),
        refresh_window_days,
        members_only: false,
        media: false,
        ..Default::default()
    })
    .await
    .unwrap();
    s.pruned
}

fn stored_ts(out: &Path) -> Vec<String> {
    let raw = block_on_load_all(&db_path_for(out)).expect("load db");
    let mut ts: Vec<String> = raw.messages.iter().map(|m| m.ts.clone()).collect();
    ts.sort();
    ts
}

/// The headline: a message that vanishes from a re-walked window is
/// deleted from our copy too.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_message_missing_from_a_rewalked_window_is_deleted() {
    let _guard = ENV_LOCK.lock().await;
    let d = tempdir().unwrap();
    let (api, playback, out) = (
        d.path().join("input_raw"),
        d.path().join("playback"),
        d.path().join("out_raw"),
    );
    write_setup_fixtures(&api);

    // Run 1: cold start, three messages.
    write_history(
        &api,
        "run-1",
        &since_ts(),
        None,
        true,
        json!([msg(TS_A, "a"), msg(TS_B, "b"), msg(TS_C, "c")]),
    );
    // Run 2: the forward walk from the watermark finds nothing new, then
    // the refresh window re-walks `[since, C]` — and B is gone from it.
    write_history(&api, "run-2", TS_C, None, false, json!([]));
    write_history(
        &api,
        "run-3",
        &since_ts(),
        Some(TS_C),
        true,
        json!([msg(TS_A, "a"), msg(TS_C, "c")]),
    );

    SlackSynth::new(&api).synthesize(&playback).unwrap();
    std::env::set_var(PLAYBACK_ENV, &playback);

    run_fetch(&out, 0).await;
    assert_eq!(
        stored_ts(&out),
        vec![TS_A.to_string(), TS_B.to_string(), TS_C.to_string()],
        "run 1 mirrors all three",
    );

    let pruned = run_fetch(&out, 3650).await;
    assert_eq!(pruned, 1, "the run must report the deletion it acted on");
    assert_eq!(
        stored_ts(&out),
        vec![TS_A.to_string(), TS_C.to_string()],
        "B is gone from a range Slack re-served in full, so it was deleted",
    );
}

/// The other half, and the one that makes the feature safe to ship: with
/// no refresh window there is no re-walk, so nothing was re-enumerated and
/// nothing may be deleted.
///
/// Without this, "prune what the walk did not return" would delete every
/// message below the resume watermark on every run — the forward walk
/// starts at the watermark and returns nothing older, which is
/// indistinguishable from "everything older was deleted" to any check that
/// does not know the walk's bounds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn no_refresh_window_means_no_prune() {
    let _guard = ENV_LOCK.lock().await;
    let d = tempdir().unwrap();
    let (api, playback, out) = (
        d.path().join("input_raw"),
        d.path().join("playback"),
        d.path().join("out_raw"),
    );
    write_setup_fixtures(&api);

    write_history(
        &api,
        "run-1",
        &since_ts(),
        None,
        true,
        json!([msg(TS_A, "a"), msg(TS_B, "b"), msg(TS_C, "c")]),
    );
    // Run 2 serves only the forward walk, which returns nothing. A prune
    // keyed on "not returned this run" would take all three.
    write_history(&api, "run-2", TS_C, None, false, json!([]));

    SlackSynth::new(&api).synthesize(&playback).unwrap();
    std::env::set_var(PLAYBACK_ENV, &playback);

    run_fetch(&out, 0).await;
    let pruned = run_fetch(&out, 0).await;

    assert_eq!(pruned, 0, "nothing was re-enumerated, so nothing may go");
    assert_eq!(
        stored_ts(&out),
        vec![TS_A.to_string(), TS_B.to_string(), TS_C.to_string()],
        "a forward walk that returned nothing is not evidence of deletion",
    );
}
