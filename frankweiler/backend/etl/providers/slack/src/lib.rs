//! Slack provider for [`frankweiler_etl`]: Download (raw API capture)
//! and Render (raw → markdown and grid_rows sidecars). The Load
//! step is provider-agnostic and lives at [`frankweiler_etl::load`].

pub mod download;
pub mod processor;
pub mod render;
pub mod synthesize;
