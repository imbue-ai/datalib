//! `datalib-etl-contact-common` — the one place a `DatalibContact` becomes
//! a document: its markdown page and grid row, for every source about
//! people (vCards, LinkedIn connections, Facebook friends).

pub mod render;
pub mod types;

pub use render::{render_all, table_rows, ContactRenderProfile, RenderSummary};
pub use types::ContactDoc;
