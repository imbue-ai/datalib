//! Provider-owned config schema for the `github_api` source (Program A goal
//! #1). Schema-only (serde + anyhow), so the orchestrator can name
//! `GithubConfig` without linking the provider.

use datalib_source_common::{LatchkeySettings, SourceCommon};
use serde::{Deserialize, Serialize};

/// The github-owned slice of a `github_api` source. `sync:` present → managed
/// (the download path); absent → render-only over an already-on-disk API
/// capture.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    pub sync: Option<GithubApiSync>,
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
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct GithubApiSync {
    #[serde(default)]
    pub refresh_window_days: Option<i64>,
    #[serde(default)]
    pub max_prs: Option<i64>,
    /// Explicit PR refs to fetch. Each entry is a paste-able reference
    /// — either `owner/repo#NUM`, `owner/repo/pull/NUM`, or a full
    /// github.com PR URL. When non-empty, discovery is skipped and only
    /// these PRs are fetched; mirrors the `conv_uuids` shape used by
    /// the other providers so URLs paste straight in from the browser.
    #[serde(default)]
    pub pull_requests: Vec<String>,
}

/// Params for the render step — no provider-specific render knobs, so
/// this is the shared bare envelope (see the per-phase params split).
pub type GithubRenderConfig = datalib_source_common::BareRenderConfig;
