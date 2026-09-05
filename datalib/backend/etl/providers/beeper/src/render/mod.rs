//! Beeper render stage.

pub mod parse;
// `render/render.rs` inside `render/` is the repo-wide stage layout, not
// an accident: the directory is the pipeline STAGE (mirroring
// `download/`), and the file is the rendering step within it, beside
// `parse.rs`. Renaming it would break the symmetry in all twelve
// providers. Allowed here rather than repo-wide so an unintentional
// inception elsewhere still fails the build.
#[allow(clippy::module_inception)]
pub mod render;

// The UUIDv5 identity recipes live in `download::schema_raw` (identity
// recipes belong next to the schema). Re-export so existing
// `crate::render::beeper_*` callers keep resolving.
pub use super::download::schema_raw::{
    beeper_event_uuid, beeper_markdown_uuid, beeper_room_uuid, beeper_user_uuid, BEEPER_UUID_NS,
};

// Period

// The period-bucketing knob is shared with the other chat providers
// (signal, whatsapp, googlechat, …) and lives in `datalib_etl`.
// Re-export at the old path so existing call sites keep compiling.
pub use datalib_etl::periodize::Period;

// Public re-exports for the sync orchestrator

pub use parse::{parse_raw_dir, ParsedBeeper};
pub use render::{render_all, RenderSummary};
