//! Golden test for `slack::translate` against the checked-in TNG-themed
//! fixture under `tests/fixtures/slack_api`. Locks in the grid_rows
//! projection — UUID derivation, thread grouping, dedup, mention
//! resolution — that the upcoming `slack-render` binary depends on.

use std::path::PathBuf;

use frankweiler_providers::slack::translate::{
    grid_rows, slack_message_uuid, slack_thread_uuid, translate_raw_dir, ts_to_iso,
};
use insta::assert_json_snapshot;
use serde_json::json;

fn fixture_root() -> PathBuf {
    // tests/fixtures/slack_api lives at the repo root, four levels up
    // from this crate's manifest dir.
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest
        .ancestors()
        .nth(3)
        .expect("repo root")
        .join("tests/fixtures/slack_api")
}

#[test]
fn ts_to_iso_round_trips_microseconds() {
    // 12604000100.000100 → year ~2369 with 100 µs.
    let iso = ts_to_iso("12604000100.000100");
    assert!(iso.ends_with("+00:00"), "got {iso:?}");
    assert!(iso.contains(".000100"), "got {iso:?}");
}

#[test]
fn translate_tng_fixture_produces_expected_lookups() {
    let t = translate_raw_dir(&fixture_root()).expect("translate");
    let ws = t.workspace.expect("workspace");
    assert_eq!(ws.team_id, "T_NCC1701D");
    assert_eq!(ws.self_user_id.as_deref(), Some("U_PICARD"));

    assert!(t.users.contains_key("U_PICARD"));
    assert!(t.users.contains_key("U_DATA"));
    assert_eq!(
        t.channels.get("C_BRIDGE").and_then(|c| c.name.as_deref()),
        Some("bridge")
    );

    // Worf's "I recommend raising shields" appears in two run files of
    // conversations.history — must collapse to one message row.
    let worf_key = ("C_BRIDGE".to_string(), "12604000400.000400".to_string());
    assert!(t.messages.contains_key(&worf_key));

    // Picard's thread root appears in both history and replies — one row.
    let picard_root = ("C_BRIDGE".to_string(), "12604000100.000100".to_string());
    let m = t.messages.get(&picard_root).expect("root present");
    assert!(m.is_thread_root);
    assert_eq!(m.effective_thread_ts, "12604000100.000100");
}

#[test]
fn translate_tng_fixture_grid_rows_snapshot() {
    let t = translate_raw_dir(&fixture_root()).expect("translate");
    let rows = grid_rows(&t);

    // Determinism check: the well-known UUIDs the Python pipeline emits
    // for these same fixture entries must match — that is the whole
    // point of pinning the namespace.
    let picard_root_uuid = slack_message_uuid("T_NCC1701D", "C_BRIDGE", "12604000100.000100");
    let picard_thread_uuid = slack_thread_uuid("T_NCC1701D", "C_BRIDGE", "12604000100.000100");

    let thread_row = rows
        .iter()
        .find(|r| r.kind == "Slack Thread" && r.conversation_uuid == picard_thread_uuid)
        .expect("Picard thread row");
    assert_eq!(thread_row.uuid, picard_thread_uuid);
    assert_eq!(thread_row.author.as_deref(), Some("Jean-Luc Picard"));
    assert_eq!(thread_row.channel.as_deref(), Some("bridge"));
    assert_eq!(thread_row.text, "Mr. Data, status report.");
    assert!(thread_row
        .qmd_path
        .as_deref()
        .unwrap()
        .starts_with("rendered_md/slack/T_NCC1701D/bridge/threads/"));

    // Root message row's uuid is distinct from its thread's uuid (so the
    // two rows don't collide on the `uuid` PK in grid_rows).
    let root_msg = rows
        .iter()
        .find(|r| r.kind == "Slack Message" && r.uuid == picard_root_uuid)
        .expect("Picard root message row");
    assert_eq!(root_msg.message_index, Some(0));
    assert_ne!(root_msg.uuid, picard_thread_uuid);

    // Stable snapshot of the full projection. Sort by (kind, when_ts, uuid)
    // so any future iteration-order drift in `translate` doesn't flap the
    // snapshot.
    let mut sortable: Vec<_> = rows.iter().map(|r| json!(r)).collect();
    sortable.sort_by_key(|v| {
        (
            v.get("kind")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            v.get("when_ts")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
            v.get("uuid")
                .and_then(|x| x.as_str())
                .unwrap_or("")
                .to_string(),
        )
    });
    assert_json_snapshot!("tng_grid_rows", sortable);
}
