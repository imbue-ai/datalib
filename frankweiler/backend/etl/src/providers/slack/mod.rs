//! Slack provider: Extract (raw API capture) + Translate (raw → markdown
//! and grid_rows sidecars). The Load step is provider-agnostic and lives
//! at [`crate::load`].

pub mod extract;
pub mod translate;
