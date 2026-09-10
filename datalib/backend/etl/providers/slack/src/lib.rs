//! Slack provider for [`datalib_etl`]: the download half — raw API
//! capture. Rendering lives in [`datalib_etl_slack_render`].

/// Every entity id this provider mints. See `docs/dev/entity_ids.md`.
pub mod ids;
pub mod ingest;
pub mod processor;
pub mod synthesize;

pub fn user_label(real_name: Option<&str>, name: Option<&str>, user_id: &str) -> String {
    real_name.or(name).unwrap_or(user_id).to_string()
}
