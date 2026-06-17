//! Signal translate stage.
//!
//! Reads the doltlite raw store the extract stage built and emits one
//! markdown document per chat. Each message becomes one line of the
//! markdown body; each chat also gets a chat-level grid_row plus one
//! grid_row per chat item so the search grid can surface individual
//! Signal messages alongside everything else.
//!
//! Scope (first pass): standardMessage text only. Stickers, view-once
//! attachments, reactions, group updates etc. are skipped silently —
//! the raw doltlite still carries the prost bytes, so a later render
//! version can crack them open without re-extracting.

pub mod parse;
pub mod render;

// The UUIDv5 identity recipes live in `extract::schema_raw` (identity
// recipes belong next to the schema). Re-export so existing
// `crate::translate::signal_*` callers keep resolving.
pub use super::extract::schema_raw::{
    signal_chat_uuid, signal_markdown_uuid, signal_message_uuid, signal_recipient_uuid,
    SIGNAL_UUID_NS,
};

pub use frankweiler_etl::periodize::Period;
pub use parse::{parse, parse_raw_dir, ParsedSignal};
pub use render::{render_all, RenderSummary, RENDER_VERSION};
