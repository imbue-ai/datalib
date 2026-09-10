//! Provider-owned config schema for the `gitlab` source (Program A goal
//! #1). Schema-only (serde + anyhow), so the orchestrator and `http` can name
//! `GitlabConfig` without linking the provider.

use datalib_source_common::{LatchkeySettings, SourceCommon};
use serde::{Deserialize, Serialize};

/// The gitlab-owned slice of a `gitlab` source. `api` is its one way
/// in; an `ingest` step without it is refused (`IngestMethods` below).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GitlabConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    /// Which latchkey identity this source mirrors. Composed only by the
    /// providers that authenticate through the `latchkey` CLI, and
    /// forwarded whole to the download client — see [`LatchkeySettings`].
    #[serde(default)]
    pub latchkey_settings: LatchkeySettings,
    #[serde(default)]
    pub api: Option<GitlabApiSync>,
}

impl GitlabConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.latchkey_settings
            .validate()
            .map_err(anyhow::Error::msg)?;
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

/// Params for the render step — no provider-specific render knobs, so
/// this is the shared bare envelope (see the per-phase params split).
pub type GitlabRenderConfig = datalib_source_common::BareRenderConfig;

impl datalib_source_common::IngestMethods for GitlabConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::origin("api")];
}
