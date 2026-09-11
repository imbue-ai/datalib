//! `datalib-etl-chat-common` — shared QMD-and-grid-rows rendering
//! for chat-style providers (Signal, WhatsApp, Beeper, …).

pub mod account;
pub mod render;
pub mod samples;
pub mod types;

pub use account::account_label;
pub use render::{render_all, RenderProfile, RenderSummary, LAYOUT_VERSION};
// Re-exported so a provider can name its `when_ts` precision without
// taking a dependency on `datalib-time` just for the enum.
pub use datalib_time::WhenTsPrecision;
pub use types::{
    ItemKind, NormalizedAttachment, NormalizedChat, NormalizedChatItem, NormalizedDoc,
    NormalizedReaction,
};
