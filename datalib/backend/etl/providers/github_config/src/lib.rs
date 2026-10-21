//! Provider-owned config schema for the `github` source (Program A goal
//! #1). Schema-only (serde + anyhow), so the orchestrator can name
//! `GithubConfig` without linking the provider.

use datalib_source_common::{LatchkeySettings, SourceCommon};
use serde::{Deserialize, Serialize};

/// This source's config stanzas, for `datalib-step help`.
pub const EXAMPLES: &[datalib_source_common::ConfigExample] = &[
    datalib_source_common::ConfigExample {
        kind: datalib_source_common::ExampleKind::Minimal,
        method: "api",
        toml: include_str!("../examples/minimal_api.toml"),
    },
    datalib_source_common::ConfigExample {
        kind: datalib_source_common::ExampleKind::Complete,
        method: "api",
        toml: include_str!("../examples/complete_api.toml"),
    },
];

/// The github-owned slice of a `github` source. `api` is its one way
/// in; an `ingest` step without it is refused (`IngestMethods` below).
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GithubConfig {
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
    pub api: Option<GithubApiSync>,
}

impl GithubConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.latchkey_settings
            .validate()
            .map_err(anyhow::Error::msg)?;
        Ok(())
    }
}

/// GitHub PR-mirror sync knobs (discovery window + explicit PR refs).
#[derive(Debug, Clone, Serialize, Deserialize, Default, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GithubApiSync {
    /// How many days back from now each run's search must have covered;
    /// a run searches only what no earlier search did. Unset or 0 sets
    /// no limit.
    #[serde(default)]
    pub refresh_window_days: Option<i64>,
    /// Fetch at most this many PRs in one run; the rest wait for the
    /// next. Unset: no cap.
    #[serde(default)]
    pub max_prs: Option<i64>,
    /// Only these PRs, each written `owner/repo#NUM` or as a github.com
    /// PR URL pasted from the browser. When non-empty, the search for
    /// your PRs is skipped and only these are fetched.
    #[serde(default)]
    pub pull_requests: Vec<String>,
}

/// Params for the render step — no provider-specific render knobs, so
/// this is the shared bare envelope (see the per-phase params split).
pub type GithubRenderConfig = datalib_source_common::BareRenderConfig;

impl datalib_source_common::IngestMethods for GithubConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::origin("api")];
}
