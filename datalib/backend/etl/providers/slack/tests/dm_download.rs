//! Whole-download tests for the `dms` / `dm_users` config knobs.
//!
//! The unit tests in `download/mod.rs` cover `conversation_types`,
//! `resolve_dm_users` and `select_targets` as pure functions. These
//! cover the part that actually moves data: that a run with `dms` off
//! never asks Slack for a DM, that a run with it on stores DM messages,
//! and that an allowlist narrows to the named person.
//!
//! Why this needs an integration test: every gate on the DM path fails
//! *silently to a no-op*. Drop the `dms` plumbing anywhere between the
//! config struct and `conversations.list` and the run still succeeds —
//! it just mirrors no DMs, which is indistinguishable from a workspace
//! that has none. Worse in the other direction: a `select_targets` that
//! ignored its allowlist would mirror DMs the config asked to leave
//! alone, and nothing downstream would report it.
//!
//! Same synth → playback → download shape as `config_change_backfill.rs`.
//! Playback is keyed on the exact param set, so the `types=` value the
//! downloader sends is itself load-bearing: the `dms = false` fixture
//! only answers the two-channel-types request, and a run that asked for
//! `im,mpim` would fail outright rather than pass quietly.
//!
//! The conversation payloads below carry the fields the live API
//! actually returns (checked 2026-08-31): an `im` has `user`,
//! `is_archived` and `is_user_deleted` but no `name` and no
//! `is_member`; an `mpim` looks like a private channel and additionally
//! carries a `members` array that includes the account itself.

use std::collections::BTreeSet;
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

/// `datetime_to_slack_ts` of 2024-01-01T00:00:00Z — the `oldest` the
/// downloader emits on a cold start with the default `since`.
const TS_SINCE: &str = "1704067200.000000";

const CHANNEL_TYPES: &str = "public_channel,private_channel";
const DM_TYPES: &str = "public_channel,private_channel,im,mpim";

fn write_envelope(path: &Path, line: &Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let mut s = serde_json::to_string(line).unwrap();
    s.push('\n');
    fs::write(path, s).unwrap();
}

fn write_auth_and_users(api: &Path) {
    write_envelope(
        &api.join("raw_api/auth.test/run-1.jsonl"),
        &json!({
            "method": "auth.test", "params": {},
            "response": {"ok": true, "user_id": "U1", "team": "Enterprise", "team_id": "T1"},
        }),
    );
    write_envelope(
        &api.join("raw_api/users.list/run-1.jsonl"),
        &json!({
            "method": "users.list",
            "params": {"limit": "200"},
            "response": {"ok": true, "members": [
                {"id": "U1", "name": "picard", "real_name": "Jean-Luc Picard"},
                {"id": "U2", "name": "riker", "real_name": "William Riker"},
                {"id": "U3", "name": "data", "real_name": "Data"},
            ]},
        }),
    );
}

/// One `conversations.list` envelope, keyed on the exact `types` the
/// downloader is expected to send.
fn write_conversations_list(api: &Path, file: &str, types: &str, conversations: Value) {
    write_envelope(
        &api.join(format!("raw_api/conversations.list/{file}.jsonl")),
        &json!({
            "method": "conversations.list",
            "params": {
                "exclude_archived": "true",
                "limit": "200",
                "types": types,
            },
            "response": {"ok": true, "channels": conversations, "has_more": false},
        }),
    );
}

/// A cold-start `conversations.history` envelope for one conversation,
/// carrying a single message so the store shows whether it was walked.
fn write_history(api: &Path, channel: &str, ts: &str, text: &str) {
    write_envelope(
        &api.join(format!("raw_api/conversations.history/{channel}.jsonl")),
        &json!({
            "method": "conversations.history",
            "params": {
                "channel": channel,
                "include_all_metadata": "true",
                "inclusive": "true",
                "limit": "200",
                "oldest": TS_SINCE,
            },
            "response": {
                "ok": true,
                "messages": [{"ts": ts, "user": "U1", "text": text}],
                "has_more": false,
            },
        }),
    );
}

/// The four conversations every DM-enabled scenario lists, in the
/// shapes the live API returns. `U1` is the account itself.
fn all_conversations() -> Value {
    json!([
        {"id": "C1", "name": "bridge", "is_channel": true, "is_member": true,
         "is_archived": false},
        // A 1:1 DM: no `name`, and — the field that would otherwise
        // filter every DM out — no `is_member`.
        {"id": "D1", "is_im": true, "user": "U2", "is_archived": false,
         "is_user_deleted": false},
        {"id": "D2", "is_im": true, "user": "U3", "is_archived": false,
         "is_user_deleted": false},
        // A group DM: a private channel that also lists its members,
        // the account included.
        {"id": "G1", "is_mpim": true, "is_channel": true, "is_private": true,
         "is_member": true, "is_archived": false,
         "name": "mpdm-picard--riker--data-1", "members": ["U1", "U2", "U3"]},
    ])
}

/// History for every conversation above. Serving all four in every
/// scenario is what makes the narrowing assertions real: a run that
/// wrongly walks a DM finds a fixture waiting and stores its message,
/// so the assertion fails instead of a missing fixture producing a
/// swallowed per-channel warning that looks like a correct skip.
fn write_all_histories(api: &Path) {
    write_history(api, "C1", "1735689600.000100", "in the channel");
    write_history(api, "D1", "1735689600.000200", "dm with riker");
    write_history(api, "D2", "1735689600.000300", "dm with data");
    write_history(api, "G1", "1735689600.000400", "group dm");
}

async fn run_fetch(out: &Path, dms: bool, dm_users: Option<Vec<&str>>) {
    fetch(FetchOptions {
        db_path: out.to_path_buf(),
        channels: None,
        since: "2024-01-01".into(),
        refresh_window_days: 0,
        members_only: false,
        media: false,
        dms,
        dm_users: dm_users.map(|v| v.into_iter().map(String::from).collect()),
        ..Default::default()
    })
    .await
    .unwrap();
}

/// The channel ids that actually have mirrored messages.
fn channels_with_messages(out: &Path) -> BTreeSet<String> {
    let raw = block_on_load_all(&db_path_for(out)).expect("load db");
    raw.messages.iter().map(|m| m.channel_id.clone()).collect()
}

fn set(ids: &[&str]) -> BTreeSet<String> {
    ids.iter().map(|s| s.to_string()).collect()
}

/// Backward compatibility, and the default every existing config gets:
/// DMs off means the request never asks for them.
///
/// The `conversations.list` fixture answers only the channel-types
/// request. If the downloader sent `im,mpim` — because someone wired
/// `dms` to default true, or dropped the flag on its way to the
/// request — playback has nothing to serve and the run fails here,
/// rather than passing while quietly mirroring the wrong thing.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dms_off_never_asks_for_direct_messages() {
    let _guard = ENV_LOCK.lock().await;
    let d = tempdir().unwrap();
    let api = d.path().join("input_raw");
    let playback = d.path().join("playback");
    let out = d.path().join("out_raw");

    write_auth_and_users(&api);
    write_conversations_list(
        &api,
        "channels-only",
        CHANNEL_TYPES,
        json!([{"id": "C1", "name": "bridge", "is_member": true, "is_archived": false}]),
    );
    write_all_histories(&api);

    SlackSynth::new(&api).synthesize(&playback).unwrap();
    std::env::set_var(PLAYBACK_ENV, &playback);

    run_fetch(&out, false, None).await;

    assert_eq!(channels_with_messages(&out), set(&["C1"]));
}

/// The headline behavior: `dms = true` lists and walks both DM shapes.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dms_on_mirrors_direct_and_group_messages() {
    let _guard = ENV_LOCK.lock().await;
    let d = tempdir().unwrap();
    let api = d.path().join("input_raw");
    let playback = d.path().join("playback");
    let out = d.path().join("out_raw");

    write_auth_and_users(&api);
    write_conversations_list(&api, "with-dms", DM_TYPES, all_conversations());
    write_all_histories(&api);

    SlackSynth::new(&api).synthesize(&playback).unwrap();
    std::env::set_var(PLAYBACK_ENV, &playback);

    run_fetch(&out, true, None).await;

    assert_eq!(
        channels_with_messages(&out),
        set(&["C1", "D1", "D2", "G1"]),
        "dms = true should mirror the channel, both 1:1 DMs and the group DM",
    );
}

/// `dm_users` narrows to the named person's 1:1 DM. Every other DM has
/// a fixture ready, so walking one lands its message and fails this.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dm_users_allowlist_narrows_to_the_named_person() {
    let _guard = ENV_LOCK.lock().await;
    let d = tempdir().unwrap();
    let api = d.path().join("input_raw");
    let playback = d.path().join("playback");
    let out = d.path().join("out_raw");

    write_auth_and_users(&api);
    write_conversations_list(&api, "with-dms", DM_TYPES, all_conversations());
    write_all_histories(&api);

    SlackSynth::new(&api).synthesize(&playback).unwrap();
    std::env::set_var(PLAYBACK_ENV, &playback);

    // By real name, to prove the allowlist resolves through the user
    // directory rather than string-matching a channel id.
    run_fetch(&out, true, Some(vec!["William Riker"])).await;

    assert_eq!(
        channels_with_messages(&out),
        set(&["C1", "D1", "G1"]),
        "Riker's 1:1 DM and the group DM he is in are both conversations \
         with Riker; Data's 1:1 (D2) is not and must be left alone",
    );
}

/// Allowlisting the account itself must not sweep in every group DM:
/// an `mpim`'s `members` includes you, so a match run against raw
/// participants rather than counterparts would mirror the lot.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dm_users_allowlist_does_not_match_the_account_itself() {
    let _guard = ENV_LOCK.lock().await;
    let d = tempdir().unwrap();
    let api = d.path().join("input_raw");
    let playback = d.path().join("playback");
    let out = d.path().join("out_raw");

    write_auth_and_users(&api);
    write_conversations_list(&api, "with-dms", DM_TYPES, all_conversations());
    write_all_histories(&api);

    SlackSynth::new(&api).synthesize(&playback).unwrap();
    std::env::set_var(PLAYBACK_ENV, &playback);

    // U1 — the `auth.test` user. Nobody is in a DM *with* themselves
    // here, so no DM is in scope.
    run_fetch(&out, true, Some(vec!["picard"])).await;

    assert_eq!(channels_with_messages(&out), set(&["C1"]));
}

/// Turning DMs on for an already-synced store has to refetch
/// `conversations.list`, even inside the six-hour sweep TTL — the
/// cached listing predates the wider `types` and contains no DM rows,
/// so honoring it would mirror nothing and report success.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turning_dms_on_relists_despite_the_sweep_ttl() {
    let _guard = ENV_LOCK.lock().await;
    let d = tempdir().unwrap();
    let api = d.path().join("input_raw");
    let playback = d.path().join("playback");
    let out = d.path().join("out_raw");

    write_auth_and_users(&api);
    write_conversations_list(
        &api,
        "channels-only",
        CHANNEL_TYPES,
        json!([{"id": "C1", "name": "bridge", "is_member": true, "is_archived": false}]),
    );
    write_conversations_list(&api, "with-dms", DM_TYPES, all_conversations());
    write_all_histories(&api);

    SlackSynth::new(&api).synthesize(&playback).unwrap();
    std::env::set_var(PLAYBACK_ENV, &playback);

    run_fetch(&out, false, None).await;
    assert_eq!(channels_with_messages(&out), set(&["C1"]));

    // Run 2, seconds later — well inside MANIFEST_TTL.
    run_fetch(&out, true, None).await;
    assert_eq!(
        channels_with_messages(&out),
        set(&["C1", "D1", "D2", "G1"]),
        "the second run must re-list under the wider `types` rather than \
         serve the cached channels-only sweep",
    );
}

/// Turning DMs back off stops walking them, and — like every other
/// narrowing in this provider — leaves what is already mirrored alone.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn turning_dms_off_stops_walking_them_without_deleting() {
    let _guard = ENV_LOCK.lock().await;
    let d = tempdir().unwrap();
    let api = d.path().join("input_raw");
    let playback = d.path().join("playback");
    let out = d.path().join("out_raw");

    write_auth_and_users(&api);
    write_conversations_list(&api, "with-dms", DM_TYPES, all_conversations());
    write_conversations_list(
        &api,
        "channels-only",
        CHANNEL_TYPES,
        json!([{"id": "C1", "name": "bridge", "is_member": true, "is_archived": false}]),
    );
    write_all_histories(&api);
    // Run 2 resumes C1 at its watermark (exclusive), which is a
    // different param set and so needs its own fixture.
    write_envelope(
        &api.join("raw_api/conversations.history/C1-resume.jsonl"),
        &json!({
            "method": "conversations.history",
            "params": {
                "channel": "C1",
                "include_all_metadata": "true",
                "inclusive": "false",
                "limit": "200",
                "oldest": "1735689600.000100",
            },
            "response": {"ok": true, "messages": [], "has_more": false},
        }),
    );

    SlackSynth::new(&api).synthesize(&playback).unwrap();
    std::env::set_var(PLAYBACK_ENV, &playback);

    run_fetch(&out, true, None).await;
    assert_eq!(channels_with_messages(&out), set(&["C1", "D1", "D2", "G1"]));

    // The DM history fixtures are still served, so a run that kept
    // walking them would succeed — the assertion is that it doesn't
    // need to, and that nothing is dropped either.
    run_fetch(&out, false, None).await;
    assert_eq!(
        channels_with_messages(&out),
        set(&["C1", "D1", "D2", "G1"]),
        "narrowing must not delete already-mirrored DMs",
    );
}
