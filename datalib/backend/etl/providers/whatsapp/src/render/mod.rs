//! WhatsApp render stage.
//!
//! Reads the `wa_*` tables the download stage built and emits one
//! markdown document per `(chat, period_key)` bucket via
//! [`datalib_etl_chat_common::render::render_all`]. Reactions
//! (the `wa_message_add_on` / `wa_message_add_on_reaction` pair)
//! render inline under their target message.
//!
//! Scope (first pass): text messages + image-like attachments +
//! reactions. Mentions, vCards, locations, quotes, system events are
//! left in the raw store unrendered until either real data drives
//! the schema work or we get a test fixture for them.

pub mod parse;
// `render/render.rs` inside `render/` is the repo-wide stage layout, not
// an accident: the directory is the pipeline STAGE (mirroring
// `download/`), and the file is the rendering step within it, beside
// `parse.rs`. Renaming it would break the symmetry in all twelve
// providers. Allowed here rather than repo-wide so an unintentional
// inception elsewhere still fails the build.
#[allow(clippy::module_inception)]
pub mod render;

// The UUIDv5 identity recipes live in `crate::schema_raw` (identity
// recipes belong next to the schema). Re-export so existing
// `crate::render::whatsapp_*` callers keep resolving.
pub use super::schema_raw::{
    whatsapp_chat_uuid, whatsapp_markdown_uuid, whatsapp_message_uuid, whatsapp_reaction_uuid,
    WHATSAPP_UUID_NS,
};

pub use datalib_etl::periodize::Period;
pub use parse::parse;
pub use render::{render_all, RENDER_VERSION};
