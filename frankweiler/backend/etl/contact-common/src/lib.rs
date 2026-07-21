//! `frankweiler-etl-contact-common` — shared QMD-and-grid-rows rendering
//! for contact-style providers (CardDAV vCards, LinkedIn connections, …).
//!
//! The sibling of [`frankweiler-etl-chat-common`]: where that crate
//! renders conversations, this one renders *people*. Each provider's
//! render stage normalizes its own row model into the
//! [`NormalizedContact`] shape this crate defines and calls
//! [`render::render_all`]. The renderer owns the cross-cutting concerns
//! every contact-style provider would otherwise reinvent:
//!
//!   * One `.md` per contact (so the qmd embedding index treats each
//!     person as its own searchable document) with frontmatter and the
//!     [`Title`]-helper-rendered H1 — including the canonical web URL
//!     (e.g. a LinkedIn profile) when the provider supplies one.
//!   * The `| Field | Value |` detail table.
//!   * Inline photo materialization into a sibling `blobs/` dir.
//!   * One [`GridRow`] per contact + the `.grid_rows.json` sidecar.
//!   * Fingerprint hashing for skip-if-unchanged.
//!   * The `on_doc_complete` callback the sync orchestrator threads
//!     through to drive its per-doc index update.
//!
//! What stays in the provider:
//!
//!   * Picking the source rows out of the raw store and parsing them.
//!   * Stable v5 UUID minting (each provider owns its namespace) — the
//!     contact's `contact_uuid` / `group_uuid` are inputs here.
//!   * Deciding which columns become [`ContactField`]s and in what order.
//!
//! Contacts are *not* event-shaped: when the source carries a timestamp
//! (vCard `REV:`, LinkedIn's "Connected On") it flows through as
//! `when_ts`; otherwise it stays `None` and we never fabricate one.
//!
//! [`Title`]: frankweiler_etl::title::Title
//! [`GridRow`]: frankweiler_schema::grid_rows::GridRow

pub mod render;
pub mod types;

pub use render::{render_all, ContactRenderProfile, RenderSummary};
pub use types::{ContactField, ContactPhoto, NormalizedContact};
