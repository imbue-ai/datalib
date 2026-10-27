//! Provider-owned config schema for the `todoist` source. Schema-only
//! (serde + anyhow), so the orchestrator and `http` can name
//! `TodoistConfig` without linking the provider.

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

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TodoistConfig {
    #[serde(default)]
    pub common: SourceCommon,
    /// Which stored latchkey account this source signs in as.
    #[serde(default)]
    pub latchkey_settings: LatchkeySettings,
    /// Todoist's API, the one way in.
    #[serde(default)]
    pub api: Option<TodoistSync>,
}

impl TodoistConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.latchkey_settings
            .validate()
            .map_err(anyhow::Error::msg)?;
        if let Some(since) = self.api.as_ref().and_then(|a| a.completed_since.as_deref()) {
            chrono::NaiveDate::parse_from_str(since, "%Y-%m-%d").map_err(|e| {
                anyhow::anyhow!("`api.completed_since` is {since:?}, not a YYYY-MM-DD date: {e}")
            })?;
        }
        Ok(())
    }
}

/// What to copy besides the open tasks, which always come.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TodoistSync {
    /// Copy the tasks you have completed, too. Todoist keeps them apart
    /// from the open ones and answers for at most three months at a
    /// time, so the first run reads back to `completed_since` a quarter
    /// at a time; later runs read only what was completed since.
    #[serde(default = "yes")]
    pub completed: bool,
    /// How far back to read completed tasks, as `YYYY-MM-DD`. Omit it to
    /// read back to the day the account joined Todoist.
    #[serde(default)]
    pub completed_since: Option<String>,
}

fn yes() -> bool {
    true
}

impl Default for TodoistSync {
    fn default() -> Self {
        Self {
            completed: true,
            completed_since: None,
        }
    }
}

/// No render knobs.
pub type TodoistRenderConfig = datalib_source_common::BareRenderConfig;

impl datalib_source_common::IngestMethods for TodoistConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::origin("api")];
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The serde defaults and the `Default` impl have to agree, or a
    /// config that omits a knob behaves differently from one built in
    /// code.
    #[test]
    fn an_empty_api_table_copies_completed_tasks_back_to_the_start() {
        let cfg: TodoistConfig = serde_json::from_str(r#"{"api":{}}"#).unwrap();
        assert!(cfg.validate().is_ok());
        let api = cfg.api.unwrap();
        assert!(api.completed);
        assert_eq!(api.completed_since, None);
        let default = TodoistSync::default();
        assert_eq!(default.completed, api.completed);
        assert_eq!(default.completed_since, api.completed_since);
    }

    #[test]
    fn completed_since_must_be_a_date() {
        let ok: TodoistConfig =
            serde_json::from_str(r#"{"api":{"completed_since":"2364-01-01"}}"#).unwrap();
        assert!(ok.validate().is_ok());
        let bad: TodoistConfig =
            serde_json::from_str(r#"{"api":{"completed_since":"stardate 41153.7"}}"#).unwrap();
        let e = bad.validate().unwrap_err().to_string();
        assert!(e.contains("completed_since"), "{e}");
    }

    #[test]
    fn an_unknown_key_is_refused() {
        assert!(serde_json::from_str::<TodoistConfig>(r#"{"api":{"projects":["x"]}}"#).is_err());
    }
}
