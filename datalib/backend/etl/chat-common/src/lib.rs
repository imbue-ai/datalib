//! `datalib-etl-chat-common` — shared QMD-and-grid-rows rendering
//! for chat-style providers (Signal, WhatsApp, Beeper, …).

mod html;
pub mod render;
pub mod samples;
pub mod types;

pub use render::{render_all, RenderProfile, RenderSummary, LAYOUT_VERSION};
pub use types::{
    ItemKind, NormalizedAttachment, NormalizedChat, NormalizedChatItem, NormalizedDoc,
    NormalizedReaction,
};
