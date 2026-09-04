//! Golden test for `slack::render` against the checked-in TNG-themed
//! fixture under `tests/fixtures/slack_api`. Locks in the grid_rows
//! projection — UUID derivation, thread grouping, dedup, mention
//! resolution.

use std::path::PathBuf;

use datalib_etl_slack::render::{parse, ts_to_iso, ts_to_ms};

fn fixture_root() -> PathBuf {
    if let Ok(d) = std::env::var("SLACK_FIXTURE_DIR") {
        return PathBuf::from(d);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/slack_api")
}

#[test]
fn ts_to_iso_round_trips_microseconds() {
    let iso = ts_to_iso("12604000100.000100").expect("a well-formed ts parses");
    assert!(iso.ends_with("+00:00"), "got {iso:?}");
    assert!(iso.contains(".000100"), "got {iso:?}");
}

/// A `ts` we cannot read must come back as `None`, never as the epoch.
/// This is the assertion that would have caught the original bug: the
/// old parser answered every one of these with
/// `1970-01-01T00:00:00.000000+00:00`, which is indistinguishable in the
/// grid from a real 1970 message.
#[test]
fn unparseable_ts_yields_none_not_the_epoch() {
    for bad in ["", "not-a-ts", "abc.123", "1728499573.xyz", "  ", "."] {
        assert_eq!(
            ts_to_iso(bad),
            None,
            "ts_to_iso({bad:?}) fabricated a stamp"
        );
        assert_eq!(ts_to_ms(bad), None, "ts_to_ms({bad:?}) fabricated a stamp");
    }
    // A real ts still parses, and the two helpers agree on the instant.
    assert_eq!(ts_to_ms("12604000100.000100"), Some(12_604_000_100_000));
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
