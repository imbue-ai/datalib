//! `datalib-etl-chat-common` — shared QMD-and-grid-rows rendering
//! for chat-style providers (Signal, WhatsApp, Beeper, …).

pub mod render;
pub mod types;

pub use render::{render_all, RenderProfile, RenderSummary};
pub use types::{
    ItemKind, NormalizedAttachment, NormalizedChat, NormalizedChatItem, NormalizedDoc,
    NormalizedReaction,
};
