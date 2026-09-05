//! JMAP provider for [`datalib_etl`]: Download (raw API capture into
//! a single doltlite db) and Render (raw → per-thread markdown +
//! `grid_rows` sidecars). The Load step is provider-agnostic and lives
//! at [`datalib_etl::load`].

pub mod download;
pub mod mailbox_labels;
pub mod probe;
pub mod processor;
pub mod render;

pub use download::db;
