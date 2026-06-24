//! Provider-owned config schema for the `gitlab_api` source (Program A goal
//! #1). Schema-only (serde + anyhow), so the orchestrator and `http` can name
//! `GitlabConfig` without linking the provider.

use serde::{Deserialize, Serialize};

/// The gitlab-owned slice of a `gitlab_api` source. `sync:` present → live
/// mirror (the extract path); absent → translate-only over an already-on-disk
/// raw store.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitlabConfig {
    #[serde(default)]
    pub sync: Option<GitlabApiSync>,
}

impl GitlabConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// GitLab MR-mirror sync knobs (refresh window + explicit MR refs).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitlabApiSync {
    #[serde(default)]
    pub refresh_window_days: Option<i64>,
    #[serde(default)]
    pub max_mrs: Option<i64>,
    /// Explicit MR refs to fetch. Each entry is a paste-able reference
    /// — either `namespace/project!IID` or a gitlab.com MR URL. When
    /// non-empty, discovery is skipped and only these MRs are fetched.
    #[serde(default)]
    pub merge_requests: Vec<String>,
}
