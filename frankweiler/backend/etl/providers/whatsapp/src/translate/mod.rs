//! WhatsApp translate stage.
//!
//! Reads the `wa_*` tables the extract stage built and emits one
//! markdown document per `(chat, period_key)` bucket via
//! [`frankweiler_etl_chat_common::render::render_all`]. Reactions
//! (the `wa_message_add_on` / `wa_message_add_on_reaction` pair)
//! render inline under their target message.
//!
//! Scope (first pass): text messages + image-like attachments +
//! reactions. Mentions, vCards, locations, quotes, system events are
//! left in the raw store unrendered until either real data drives
//! the schema work or we get a test fixture for them.

pub mod blob_reader;
pub mod cursor;
pub mod parse;
pub mod render;

use uuid::Uuid;

/// v5 namespace for every UUID this provider mints. The bytes spell
/// `whatsapp:msgstr:` to keep it human-recognizable in dumps.
pub const WHATSAPP_UUID_NS: Uuid = Uuid::from_bytes([
    0x77, 0xa7, 0x59, 0xc0, 0xba, 0xc1, 0x4e, 0x6f, 0x9f, 0x8a, 0x73, 0x16, 0xc7, 0xba, 0xc7, 0xc0,
]);

pub fn whatsapp_chat_uuid(source: &str, chat_jid: &str) -> String {
    Uuid::new_v5(
        &WHATSAPP_UUID_NS,
        format!("whatsapp:chat:{source}:{chat_jid}").as_bytes(),
    )
    .to_string()
}

pub fn whatsapp_message_uuid(source: &str, chat_jid: &str, key_id: &str, from_me: i64) -> String {
    Uuid::new_v5(
        &WHATSAPP_UUID_NS,
        format!("whatsapp:msg:{source}:{chat_jid}:{key_id}:{from_me}").as_bytes(),
    )
    .to_string()
}

pub fn whatsapp_reaction_uuid(source: &str, chat_jid: &str, key_id: &str, from_me: i64) -> String {
    Uuid::new_v5(
        &WHATSAPP_UUID_NS,
        format!("whatsapp:react:{source}:{chat_jid}:{key_id}:{from_me}").as_bytes(),
    )
    .to_string()
}

/// Per-bucket document UUID. Stable for the lifetime of a
/// `(chat, period_key)` pair regardless of how many times we re-render.
pub fn whatsapp_markdown_uuid(chat_uuid: &str, period_key: &str) -> String {
    Uuid::new_v5(
        &WHATSAPP_UUID_NS,
        format!("whatsapp:doc:{chat_uuid}:{period_key}").as_bytes(),
    )
    .to_string()
}

pub use frankweiler_etl::periodize::Period;
pub use parse::parse;
pub use render::{render_all, RENDER_VERSION};
