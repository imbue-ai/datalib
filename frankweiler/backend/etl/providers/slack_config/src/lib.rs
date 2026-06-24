//! Provider-owned config schema for the `slack_api` source (Program A goal #1).
//! Schema-only (serde + anyhow).

use frankweiler_source_common::SourceCommon;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SlackConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    #[serde(default)]
    pub sync: Option<SlackApiSync>,
}

impl SlackConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SlackApiSync {
    #[serde(default)]
    pub refresh_window_days: Option<i64>,
    #[serde(default)]
    pub channels: Option<Vec<String>>,
    #[serde(default)]
    pub since: Option<String>,
    #[serde(default)]
    pub all_channels: bool,
    #[serde(default = "default_true")]
    pub media: bool,
}

impl Default for SlackApiSync {
    fn default() -> Self {
        Self {
            refresh_window_days: None,
            channels: None,
            since: None,
            all_channels: false,
            media: true,
        }
    }
}
