//! Beeper render stage.
//!
//! Reads the doltlite raw store the download stage built and emits
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

// The UUIDv5 identity recipes live in `download::schema_raw` (identity
// recipes belong next to the schema). Re-export so existing
// `crate::render::beeper_*` callers keep resolving.
pub use super::download::schema_raw::{
    beeper_event_uuid, beeper_markdown_uuid, beeper_room_uuid, beeper_user_uuid, BEEPER_UUID_NS,
};

// ─────────────────────────────────────────────────────────────────────
// Period
// ─────────────────────────────────────────────────────────────────────

// The period-bucketing knob is shared with the other chat providers
// (signal, whatsapp, googlechat, …) and lives in `frankweiler_etl`.
// Re-export at the old path so existing call sites keep compiling.
pub use frankweiler_etl::periodize::Period;

// ─────────────────────────────────────────────────────────────────────
// Public re-exports for the sync orchestrator
// ─────────────────────────────────────────────────────────────────────

pub use parse::{parse_raw_dir, ParsedBeeper};
pub use render::{render_all, RenderSummary};
