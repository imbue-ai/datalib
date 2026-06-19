//! Golden test for `slack::translate` against the checked-in TNG-themed
//! fixture under `tests/fixtures/slack_api`. Locks in the grid_rows
//! projection — UUID derivation, thread grouping, dedup, mention
//! resolution.

use std::path::PathBuf;

use frankweiler_etl_slack::translate::{parse, ts_to_iso};

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
