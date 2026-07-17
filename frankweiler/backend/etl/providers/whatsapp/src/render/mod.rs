//! WhatsApp render stage.
//!
//! Reads the `wa_*` tables the download stage built and emits one
//! markdown document per `(chat, period_key)` bucket via
//! [`frankweiler_etl_chat_common::render::render_all`]. Reactions
//! (the `wa_message_add_on` / `wa_message_add_on_reaction` pair)
//! render inline under their target message.
//!
//! Scope (first pass): text messages + image-like attachments +
//! reactions. Mentions, vCards, locations, quotes, system events are
//! left in the raw store unrendered until either real data drives
//! the schema work or we get a test fixture for them.

pub mod parse;
pub mod render;

// The UUIDv5 identity recipes live in `crate::schema_raw` (identity
// recipes belong next to the schema). Re-export so existing
// `crate::render::whatsapp_*` callers keep resolving.
pub use super::schema_raw::{
    whatsapp_chat_uuid, whatsapp_markdown_uuid, whatsapp_message_uuid, whatsapp_reaction_uuid,
    WHATSAPP_UUID_NS,
};

pub use frankweiler_etl::periodize::Period;
pub use parse::parse;
pub use render::{render_all, RENDER_VERSION};
