//! Golden test for `slack::translate` against the checked-in TNG-themed
//! fixture under `tests/fixtures/slack_api`. Locks in the grid_rows
//! projection — UUID derivation, thread grouping, dedup, mention
//! resolution.

use std::path::PathBuf;

use frankweiler_etl_slack::translate::{
    grid_rows, parse, slack_message_uuid, slack_thread_uuid, ts_to_iso,
};
use insta::assert_json_snapshot;
use serde_json::json;

fn fixture_root() -> PathBuf {
    if let Ok(d) = std::env::var("SLACK_FIXTURE_DIR") {
        return PathBuf::from(d);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/slack_api")
}

#[test]
fn ts_to_iso_round_trips_microseconds() {
    let iso = ts_to_iso("12604000100.000100");
    assert!(iso.ends_with("+00:00"), "got {iso:?}");
    assert!(iso.contains(".000100"), "got {iso:?}");
}

#[test]
fn translate_tng_fixture_produces_expected_lookups() {
    let t = parse(&fixture_root(), None).expect("parse");
    let ws = t.workspace.as_ref().expect("workspace");
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
    let worf_present = t
        .threads
        .iter()
        .flat_map(|b| b.messages.iter())
        .any(|m| m.channel_id == "C_BRIDGE" && m.ts == "12604000400.000400");
    assert!(worf_present, "Worf message must be present");

    // Picard's thread root appears in both history and replies — one row.
    let picard_root = t
        .threads
        .iter()
        .flat_map(|b| b.messages.iter())
        .find(|m| m.channel_id == "C_BRIDGE" && m.ts == "12604000100.000100")
        .expect("Picard root present");
    assert!(picard_root.is_thread_root);
    assert_eq!(picard_root.effective_thread_ts, "12604000100.000100");
}

#[test]
fn translate_tng_fixture_grid_rows_snapshot() {
    let t = parse(&fixture_root(), None).expect("parse");
    let rows = grid_rows(&t);

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
        .starts_with("rendered_md/slack/T_NCC1701D/C_BRIDGE/threads/"));

    let root_msg = rows
        .iter()
        .find(|r| r.kind == "Slack Message" && r.uuid == picard_root_uuid)
        .expect("Picard root message row");
    assert_eq!(root_msg.message_index, Some(0));
    assert_ne!(root_msg.uuid, picard_thread_uuid);

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
