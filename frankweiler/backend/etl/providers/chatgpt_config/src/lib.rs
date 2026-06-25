//! Provider-owned config schema for the `chatgpt_api` source (Program A goal
//! #1). Schema-only (serde + anyhow).

use frankweiler_source_common::SourceCommon;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ChatgptConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    #[serde(default)]
    pub sync: Option<ChatgptApiSync>,
}

impl ChatgptConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ChatgptApiSync {
    #[serde(default)]
    pub refresh_window_days: Option<i64>,
    #[serde(default)]
    pub max_pages: Option<i64>,
    #[serde(default)]
    pub limit: Option<i64>,
    #[serde(default)]
    pub sleep_between: Option<f64>,
    /// When non-empty, restrict the fetch to exactly these conversation ids
    /// (bare id or a paste-able `https://chatgpt.com/c/<id>` URL).
    #[serde(default)]
    pub conv_uuids: Vec<String>,
}
