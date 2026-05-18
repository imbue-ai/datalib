//! Slack provider for [`frankweiler_etl`]: Extract (raw API capture)
//! and Translate (raw → markdown and grid_rows sidecars). The Load
//! step is provider-agnostic and lives at [`frankweiler_etl::load`].

pub mod extract;
pub mod synthesize;
pub mod translate;
