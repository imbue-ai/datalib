//! Provider-owned config schema for the `gitlab` source (Program A goal
//! #1). Schema-only (serde + anyhow), so the orchestrator and `http` can name
//! `GitlabConfig` without linking the provider.

use datalib_source_common::{not_negative, LatchkeySettings, SourceCommon};
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

/// The gitlab-owned slice of a `gitlab` source. `api` is its one way
/// in; an `ingest` step without it is refused (`IngestMethods` below).
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
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
        if let Some(api) = &self.api {
            not_negative("api.refresh_window_days", api.refresh_window_days)
                .map_err(anyhow::Error::msg)?;
            not_negative("api.max_mrs", api.max_mrs).map_err(anyhow::Error::msg)?;
        }
        Ok(())
    }
}

/// GitLab MR-mirror sync knobs (refresh window + explicit MR refs).
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GitlabApiSync {
    /// How many days back from now each run's listing must have
    /// covered; a run lists only what no earlier listing did. Unset or 0
    /// sets no limit.
    #[serde(default)]
    pub refresh_window_days: Option<i64>,
    /// Fetch at most this many MRs in one run; the rest wait for the
    /// next. Unset: no cap.
    #[serde(default)]
    pub max_mrs: Option<i64>,
    /// Only these MRs, each written `namespace/project!IID` or as a
    /// gitlab.com MR URL. When non-empty, the search for your MRs is
    /// skipped and only these are fetched.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A negative cap was cast `as usize` into one too big to reach,
    /// and a negative window clamped to 0, which means no limit.
    #[test]
    fn negative_counts_are_refused_by_name() {
        for (window, cap, named) in [
            (Some(-1), None, "api.refresh_window_days"),
            (None, Some(-5), "api.max_mrs"),
        ] {
            let mut cfg = GitlabConfig::default();
            let mut api = GitlabApiSync::default();
            api.refresh_window_days = window;
            api.max_mrs = cap;
            cfg.api = Some(api);
            let err = cfg.validate().unwrap_err().to_string();
            assert!(err.contains(named), "{err}");
        }
        let mut cfg = GitlabConfig::default();
        cfg.api = Some(GitlabApiSync {
            refresh_window_days: Some(0),
            max_mrs: Some(0),
            ..Default::default()
        });
        assert!(cfg.validate().is_ok());
    }
}
