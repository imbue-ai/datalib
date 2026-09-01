//! Slack provider for [`datalib_etl`]: Download (raw API capture)
//! and Render (raw → markdown and grid_rows sidecars). The Load
//! step is provider-agnostic and lives at [`datalib_etl::load`].

pub mod download;
/// Every entity id this provider mints. See `docs/dev/entity_ids.md`.
pub mod ids;
pub mod processor;
pub mod render;
pub mod synthesize;

/// The most human of a Slack user's names.
///
/// One rule, two callers on opposite sides of the pipeline: the
/// downloader labels DM progress lines with it (`download::db`), and
/// the renderer titles messages and conversations with it
/// (`render::User::label`). Letting them drift would mean a DM
/// announced as `@riker` while syncing and titled `@William Riker`
/// once rendered.
pub fn user_label(real_name: Option<&str>, name: Option<&str>, user_id: &str) -> String {
    real_name.or(name).unwrap_or(user_id).to_string()
}
