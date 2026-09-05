//! `datalib-etl-contact-common` — shared QMD-and-grid-rows rendering
//! for contact-style providers (CardDAV vCards, LinkedIn connections, …).

pub mod render;
pub mod types;

pub use render::{render_all, ContactRenderProfile, RenderSummary};
pub use types::{ContactField, ContactPhoto, NormalizedContact};
