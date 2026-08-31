//! Slack provider for [`datalib_etl`]: Download (raw API capture)
//! and Render (raw → markdown and grid_rows sidecars). The Load
//! step is provider-agnostic and lives at [`datalib_etl::load`].

pub mod download;
/// Every entity id this provider mints. See `docs/dev/entity_ids.md`.
pub mod ids;
pub mod processor;
pub mod render;
pub mod synthesize;
