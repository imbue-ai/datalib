//! Provider-owned config schema for the `linear` source. Schema-only
//! (serde + anyhow), so the orchestrator and `http` can name
//! `LinearConfig` without linking the provider.

use datalib_source_common::{LatchkeySettings, SourceCommon};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinearConfig {
    #[serde(default)]
    pub common: SourceCommon,
    /// Which latchkey identity this source mirrors.
    #[serde(default)]
    pub latchkey_settings: LatchkeySettings,
    /// Linear's GraphQL API, the one way in.
    #[serde(default)]
    pub api: Option<LinearSync>,
}

impl LinearConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.latchkey_settings
            .validate()
            .map_err(anyhow::Error::msg)?;
        Ok(())
    }
}

/// Which part of the workspace to mirror. Both lists empty is the whole
/// workspace. Otherwise an issue is mirrored when its team is named in
/// `teams` **or** its project in `projects`; most issues belong to no
/// project, so a list of projects alone leaves them out.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinearSync {
    /// Teams, by key (`ENG`, matched without regard to case) or by name.
    #[serde(default)]
    pub teams: Vec<String>,
    /// Projects, by name, as Linear shows it.
    #[serde(default)]
    pub projects: Vec<String>,
}

impl LinearSync {
    pub fn whole_workspace(&self) -> bool {
        self.teams.is_empty() && self.projects.is_empty()
    }
}

/// No render knobs: `linear` renders nothing.
pub type LinearRenderConfig = datalib_source_common::BareRenderConfig;

impl datalib_source_common::IngestMethods for LinearConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::origin("api")];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_api_table_is_the_whole_workspace() {
        let cfg: LinearConfig = serde_json::from_str(r#"{"api":{}}"#).unwrap();
        assert!(cfg.validate().is_ok());
        assert!(cfg.api.unwrap().whole_workspace());
    }

    #[test]
    fn naming_a_team_or_a_project_narrows_it() {
        let cfg: LinearConfig =
            serde_json::from_str(r#"{"api":{"teams":["ENG"],"projects":["Warp core refit"]}}"#)
                .unwrap();
        let api = cfg.api.unwrap();
        assert!(!api.whole_workspace());
        assert_eq!(api.teams, ["ENG"]);
        assert_eq!(api.projects, ["Warp core refit"]);
    }

    #[test]
    fn an_unknown_key_is_refused() {
        assert!(serde_json::from_str::<LinearConfig>(r#"{"api":{"channels":["x"]}}"#).is_err());
    }
}
