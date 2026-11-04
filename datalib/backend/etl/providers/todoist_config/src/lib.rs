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
        if let Some(api) = &self.api {
            datalib_time::TimeWindow::check("api", api.since.as_deref(), None)
                .map_err(anyhow::Error::msg)?;
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
    /// time, so the first run reads back to `since` a quarter at a time;
    /// later runs read only what was completed since.
    #[serde(default = "yes")]
    pub completed: bool,
    /// How far back to read completed tasks: a day (`2024-01-01`), an
    /// RFC 3339 instant, or an offset from the run's start (`-90d`). Omit
    /// it to read back to the day the account joined Todoist.
    #[serde(default)]
    pub since: Option<String>,
}

fn yes() -> bool {
    true
}

impl Default for TodoistSync {
    fn default() -> Self {
        Self {
            completed: true,
            since: None,
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
        assert_eq!(api.since, None);
        let default = TodoistSync::default();
        assert_eq!(default.completed, api.completed);
        assert_eq!(default.since, api.since);
    }

    /// `api.since` takes the window grammar every source shares, so an
    /// offset loads, and a typo is refused at load naming the key.
    #[test]
    fn since_takes_the_shared_window_grammar() {
        let since = |s: &str| {
            let cfg: TodoistConfig =
                serde_json::from_str(&format!(r#"{{"api":{{"since":"{s}"}}}}"#)).unwrap();
            cfg.validate()
        };
        for ok in ["2364-01-01", "2364-01-01T09:00:00Z", "1900-01-01", "-90d"] {
            since(ok).unwrap_or_else(|e| panic!("{ok}: {e}"));
        }
        let e = since("stardate 41153.7").unwrap_err().to_string();
        assert!(e.contains("`api.since`"), "{e}");
    }

    /// The window's old name is refused, so a config `datalib-http` has
    /// not yet brought forward (`upgrade_todoist_since`) fails loudly
    /// rather than reading back to the day the account joined.
    #[test]
    fn an_unknown_key_is_refused() {
        assert!(serde_json::from_str::<TodoistConfig>(r#"{"api":{"projects":["x"]}}"#).is_err());
        let old = r#"{"api":{"completed_since":"2364-01-01"}}"#;
        assert!(serde_json::from_str::<TodoistConfig>(old).is_err());
    }
}
