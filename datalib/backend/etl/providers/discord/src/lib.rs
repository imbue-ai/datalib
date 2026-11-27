//! Discord provider for [`datalib_etl`]: the download half, which mirrors
//! what the signed-in account can read from `discord.com` into raw JSON
//! tables. The render half is `datalib_etl_discord_render`; INGEST.md has
//! the full description.

pub mod ingest;
pub mod probe;
pub mod processor;
pub mod synthesize;

/// The name a person is shown under: the display name they chose, else
/// their username, else the bare id.
pub fn user_label(global_name: Option<&str>, username: Option<&str>, id: &str) -> String {
    [global_name, username]
        .into_iter()
        .flatten()
        .find(|s| !s.trim().is_empty())
        .unwrap_or(id)
        .to_string()
}
