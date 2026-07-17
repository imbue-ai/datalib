//! `frankweiler-etl-chat-common` — shared QMD-and-grid-rows rendering
//! for chat-style providers (Signal, WhatsApp, Beeper, …).
//!
//! Each provider's render stage normalizes its own row model into
//! the [`NormalizedChat`] / [`NormalizedDoc`] / [`NormalizedItem`]
//! shape this crate defines and calls [`render::render_all`]. The
//! renderer owns the cross-cutting concerns every chat-style provider
//! would otherwise reinvent:
//!
//!   * `.md` frontmatter and the [`Title`]-helper-rendered H1.
//!   * The per-message block with its `id="m-{uuid}"` anchor.
//!   * Attachment rendering (link-only — the provider materializes
//!     blob bytes before calling render_all and supplies the relative
//!     path the markdown points at).
//!   * Inline reactions grouped under their target message.
//!   * Chat- and message-level [`GridRow`] sidecar emission.
//!   * Fingerprint hashing for skip-if-unchanged.
//!   * The `on_doc_complete` callback the sync orchestrator threads
//!     through to drive its per-doc index update.
//!
//! What stays in the provider:
//!
//!   * Picking the source rows out of the raw store and grouping them
//!     into `(chat, period_key)` buckets.
//!   * Author / participant name resolution.
//!   * Stable v5 UUID minting (each provider has its own namespace).
//!   * Blob materialization — copying / linking files from the source
//!     into `<page_dir>/blobs/` *before* calling [`render::render_all`],
//!     then handing chat-common the relative paths via
//!     [`NormalizedAttachment::rel_path`].
//!
//! The renderer does NOT distinguish bullets-vs-blocks via a knob; it
//! uses a single block style with `<div id="m-{uuid}" …>` wrappers,
//! matching Beeper's existing layout. Signal and WhatsApp adopting
//! this layout means a one-time visible shift in their rendered
//! markdown (snapshots regenerated alongside this commit).
//!
//! [`Title`]: frankweiler_etl::title::Title
//! [`GridRow`]: frankweiler_schema::grid_rows::GridRow

pub mod render;
pub mod types;

pub use render::{render_all, RenderProfile, RenderSummary};
pub use types::{
    ItemKind, NormalizedAttachment, NormalizedChat, NormalizedChatItem, NormalizedDoc,
    NormalizedReaction,
};
