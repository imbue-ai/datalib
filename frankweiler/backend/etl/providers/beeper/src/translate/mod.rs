//! Beeper translate stage: raw Matrix events → grid_rows + rendered
//! markdown. Dispatches per-room on the room's inferred
//! `bridge_network` (the multiplex key), since iMessage/WhatsApp/
//! Signal/etc. all arrive as Matrix events but carry per-bridge
//! quirks (tapbacks, edits, reply quoting).
//!
//! Milestone A: no translators wired up yet. The sync orchestrator's
//! Beeper translate arm calls [`parse_raw_dir`] and a render path that
//! emits zero docs. Real bridges come online in Milestones C/D.

use std::path::Path;

use anyhow::Result;
use uuid::Uuid;

/// Shared namespace for v5-derived Beeper UUIDs. New constant — not
/// shared with slack/notion/etc. so a future cutover doesn't collide
/// on `uuid` with a different provider's rows.
pub const BEEPER_UUID_NS: Uuid = Uuid::from_bytes([
    0xbe, 0xe9, 0xe7, 0x00, 0x4f, 0x3d, 0x5a, 0x6b, 0x9f, 0x8a, 0x3e, 0x3d, 0x5a, 0x6b, 0x9f, 0x8a,
]);

pub fn beeper_room_uuid(matrix_room_id: &str) -> String {
    Uuid::new_v5(
        &BEEPER_UUID_NS,
        format!("beeper:room:{matrix_room_id}").as_bytes(),
    )
    .to_string()
}

pub fn beeper_user_uuid(matrix_user_id: &str) -> String {
    Uuid::new_v5(
        &BEEPER_UUID_NS,
        format!("beeper:user:{matrix_user_id}").as_bytes(),
    )
    .to_string()
}

pub fn beeper_event_uuid(matrix_room_id: &str, event_id: &str) -> String {
    Uuid::new_v5(
        &BEEPER_UUID_NS,
        format!("beeper:event:{matrix_room_id}:{event_id}").as_bytes(),
    )
    .to_string()
}

/// Placeholder for Milestone C — parses the doltlite raw store into
/// the in-memory shape the renderers consume.
#[derive(Debug, Default)]
pub struct ParsedBeeper {}

pub fn parse_raw_dir(_input: &Path) -> Result<ParsedBeeper> {
    Ok(ParsedBeeper::default())
}
