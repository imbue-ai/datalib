//! `datalib-etl-contact-common` — the one place a `NormalizedContact` becomes
//! a document: its markdown page and grid row, for every source about
//! people (vCards, LinkedIn connections, Facebook friends).

pub mod render;
pub mod samples;
pub mod types;

pub use render::{
    chip_handle, field_rows, render_all, ContactRenderProfile, FieldRow, RenderSummary,
};
pub use types::ContactDoc;
