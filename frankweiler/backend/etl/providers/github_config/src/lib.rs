//! Provider-owned config schema for the `github_api` source (Program A goal
//! #1). Schema-only (serde + anyhow), so the orchestrator can name
//! `GithubConfig` without linking the provider.

use serde::{Deserialize, Serialize};

/// The github-owned slice of a `github_api` source. `sync:` present → managed
/// (the extract path); absent → translate-only over an already-on-disk API
/// capture.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GithubConfig {
    #[serde(default)]
    pub sync: Option<GithubApiSync>,
}

impl GithubConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
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
