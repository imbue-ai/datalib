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

use uuid::Uuid;

/// v5 namespace for every UUID this provider mints. The bytes spell
/// `signal:backup:` to keep it human-recognizable in dumps.
pub const SIGNAL_UUID_NS: Uuid = Uuid::from_bytes([
    0x51, 0x91, 0xa1, 0x00, 0xba, 0xc1, 0x4e, 0x6f, 0x9f, 0x8a, 0x53, 0x16, 0xa1, 0xba, 0xc1, 0x4e,
]);

pub fn signal_chat_uuid(source: &str, chat_id: &str) -> String {
    Uuid::new_v5(
        &SIGNAL_UUID_NS,
        format!("signal:chat:{source}:{chat_id}").as_bytes(),
    )
    .to_string()
}

pub fn signal_recipient_uuid(source: &str, recipient_id: &str) -> String {
    Uuid::new_v5(
        &SIGNAL_UUID_NS,
        format!("signal:recipient:{source}:{recipient_id}").as_bytes(),
    )
    .to_string()
}

pub fn signal_message_uuid(source: &str, chat_id: &str, author_id: &str, date_sent: i64) -> String {
    Uuid::new_v5(
        &SIGNAL_UUID_NS,
        format!("signal:msg:{source}:{chat_id}:{author_id}:{date_sent}").as_bytes(),
    )
    .to_string()
}

pub use parse::{parse_raw_dir, ParsedSignal};
pub use render::{render_all, RenderSummary, RENDER_VERSION};
