//! Slack provider for [`datalib_etl`]: Download (raw API capture)
//! and Render (raw → markdown and grid_rows in the render store). The Load
//! step is provider-agnostic and lives at [`datalib_etl::load`].

pub mod download;
/// Every entity id this provider mints. See `docs/dev/entity_ids.md`.
pub mod ids;
pub mod processor;
pub mod render;
pub mod synthesize;

pub fn user_label(real_name: Option<&str>, name: Option<&str>, user_id: &str) -> String {
    real_name.or(name).unwrap_or(user_id).to_string()
}
