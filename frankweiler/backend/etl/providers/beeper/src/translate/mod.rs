//! Beeper translate stage.
//!
//! Reads the doltlite raw store the extract stage built and emits
//! one markdown document per `(room, period)`. The period is
//! configurable — `month` (default), `day`, `year`, or `all` —
//! and gives each chat a per-time-bucket file you can read like a
//! transcript.
//!
//! Reactions render in the period of the message they target, not
//! in their own period, so a June reaction on a May message
//! triggers a rewrite of the May document. That's idempotent —
//! the document's `source_fingerprint` includes both messages and
//! attached reactions, so re-runs converge.

pub mod parse;
pub mod render;

use anyhow::Result;
use uuid::Uuid;

/// v5 namespace for every UUID this provider mints. Distinct from
/// other providers so we can never accidentally collide a Beeper
/// row with a Slack/Notion/etc. row that happened to derive its
/// id from the same string.
pub const BEEPER_UUID_NS: Uuid = Uuid::from_bytes([
    0xbe, 0xe9, 0xe7, 0x00, 0x4f, 0x3d, 0x5a, 0x6b, 0x9f, 0x8a, 0x3e, 0x3d, 0x5a, 0x6b, 0x9f, 0x8a,
]);

/// `source` is the on-disk store the row came from (e.g.
/// `"beeper_index"`, eventually `"macos_imessage"`). Including it
/// in the v5 hash means two extractors that happen to mint
/// identical native ids never collide unless that's actually
/// meaningful.
pub fn beeper_room_uuid(source: &str, native_room_id: &str) -> String {
    Uuid::new_v5(
        &BEEPER_UUID_NS,
        format!("beeper:room:{source}:{native_room_id}").as_bytes(),
    )
    .to_string()
}

pub fn beeper_user_uuid(source: &str, native_user_id: &str) -> String {
    Uuid::new_v5(
        &BEEPER_UUID_NS,
        format!("beeper:user:{source}:{native_user_id}").as_bytes(),
    )
    .to_string()
}

pub fn beeper_event_uuid(source: &str, native_event_id: &str) -> String {
    Uuid::new_v5(
        &BEEPER_UUID_NS,
        format!("beeper:event:{source}:{native_event_id}").as_bytes(),
    )
    .to_string()
}

/// Per-period document UUID. Stable for the lifetime of the
/// `(room, period)` pair regardless of how many times we re-render
/// — so the load step can foreign-key against it consistently.
pub fn beeper_document_uuid(room_uuid: &str, period_key: &str) -> String {
    Uuid::new_v5(
        &BEEPER_UUID_NS,
        format!("beeper:doc:{room_uuid}:{period_key}").as_bytes(),
    )
    .to_string()
}

// ─────────────────────────────────────────────────────────────────────
// Period
// ─────────────────────────────────────────────────────────────────────

/// How we bucket a room's events into rendered documents. Each
/// variant ultimately becomes a SQLite `strftime` format string
/// that maps a UTC timestamp to the period's textual key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Period {
    Month,
    Day,
    Year,
    All,
}

impl Period {
    pub fn from_config(s: Option<&str>) -> Result<Self> {
        Ok(match s.unwrap_or("month").to_ascii_lowercase().as_str() {
            "month" => Period::Month,
            "day" => Period::Day,
            "year" => Period::Year,
            "all" => Period::All,
            other => anyhow::bail!(
                "unknown beeper period {other:?}; expected one of: month, day, year, all"
            ),
        })
    }

    /// SQLite format string passed to `strftime(<fmt>, ts/1000,
    /// 'unixepoch')`. For `All`, we use a literal so every row
    /// buckets together.
    pub fn strftime_fmt(self) -> &'static str {
        match self {
            Period::Month => "%Y-%m",
            Period::Day => "%Y-%m-%d",
            Period::Year => "%Y",
            // Sentinel — every row in a single bucket. We map this
            // to a constant key at SELECT time.
            Period::All => "%Y-%m-%dT%H:%M:%S",
        }
    }

    pub fn key_for_all() -> &'static str {
        "all"
    }
}

// ─────────────────────────────────────────────────────────────────────
// Public re-exports for the sync orchestrator
// ─────────────────────────────────────────────────────────────────────

pub use parse::{parse_raw_dir, ParsedBeeper};
pub use render::{render_all, RenderSummary};
