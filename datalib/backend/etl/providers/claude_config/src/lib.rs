//! Provider-owned config schema for the `claude` source (Program A goal
//! #1). Schema-only (serde + anyhow), so the orchestrator and `http` can
//! name `ClaudeConfig` without linking the provider.

use datalib_source_common::{LatchkeySettings, LocalPath, SourceCommon};
use serde::{Deserialize, Serialize};

/// The Claude-owned slice of a `claude` source. Two ways in, one raw
/// store: `api` walks the live claude.ai API, `export` reads an unpacked
/// bulk export off disk. Exactly one is set; an `ingest` step with
/// neither is refused (`IngestMethods` below), and one with both is
/// refused by [`ClaudeConfig::validate`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeConfig {
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
    pub api: Option<ClaudeApiSync>,
    /// The directory a Claude data export was unpacked into — the one
    /// holding `conversations.json`. Ingested as a whole snapshot: what
    /// the export no longer has is pruned from the store.
    #[serde(default)]
    pub export: Option<LocalPath>,
}

impl ClaudeConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.latchkey_settings
            .validate()
            .map_err(anyhow::Error::msg)?;
        if self.api.is_some() && self.export.is_some() {
            anyhow::bail!(
                "claude sets both `api` and `export` — pick one. To seed a store from an \
                 export and then keep it fresh from the API, see the provider's INGEST.md."
            );
        }
        Ok(())
    }
}

/// JMAP-less Claude.ai sync knobs (conversation refresh + explicit UUIDs).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeApiSync {
    /// Only sync conversations whose `updated_at` is at or after this
    /// instant (RFC 3339 or `YYYY-MM-DD`, assumed UTC). Older
    /// conversations are never detail-fetched; moving the date further
    /// back later backfills them on the next run. Unset → sync
    /// everything.
    #[serde(default)]
    pub since: Option<String>,
    /// Force-refetch the N most-recently-updated conversations each run.
    #[serde(default)]
    pub refresh_most_recent_n_chat_count: Option<i64>,
    /// When non-empty, restrict the fetch to exactly these conversation UUIDs
    /// (bare UUID or a paste-able `https://claude.ai/chat/<uuid>` URL).
    #[serde(default)]
    pub conv_uuids: Vec<String>,
    /// Also mirror Claude Projects: each project's description, custom
    /// instructions, and knowledge documents. On by default — it costs
    /// one extra request per org plus one per project whose knowledge
    /// needs refreshing, and a project is the only place some of a
    /// user's written context lives.
    #[serde(default = "default_true")]
    pub projects: bool,
    /// When non-empty, restrict the project mirror to exactly these
    /// project UUIDs (bare UUID or a paste-able
    /// `https://claude.ai/project/<uuid>` URL). The per-org listing
    /// still runs — it is one request and it is where the metadata
    /// comes from — but every project outside this set is left alone.
    #[serde(default)]
    pub project_uuids: Vec<String>,
}

fn default_true() -> bool {
    true
}

impl Default for ClaudeApiSync {
    fn default() -> Self {
        Self {
            since: None,
            refresh_most_recent_n_chat_count: None,
            conv_uuids: Vec::new(),
            projects: default_true(),
            project_uuids: Vec::new(),
        }
    }
}

/// Params for the render step.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeRenderConfig {
    #[serde(default)]
    pub common: datalib_source_common::RenderCommon,

    /// Truncate a project knowledge document's inline text at this many
    /// bytes when rendering it into the project's page.
    #[serde(default = "default_max_project_doc_bytes")]
    pub max_project_doc_bytes: Option<usize>,
}

fn default_max_project_doc_bytes() -> Option<usize> {
    Some(128 * 1024)
}

impl Default for ClaudeRenderConfig {
    fn default() -> Self {
        Self {
            common: Default::default(),
            max_project_doc_bytes: default_max_project_doc_bytes(),
        }
    }
}

impl datalib_source_common::IngestMethods for ClaudeConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] = &[
        datalib_source_common::IngestMethod::origin("api"),
        datalib_source_common::IngestMethod::local("export"),
    ];
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The serde default and the `Default` impl have to agree, or a
    /// config that omits `projects` behaves differently from one built
    /// in Rust. Easy to break by adding a field to only one of them.
    #[test]
    fn projects_defaults_on_both_ways() {
        let from_toml: ClaudeApiSync = toml::from_str("").unwrap();
        assert!(from_toml.projects);
        assert!(ClaudeApiSync::default().projects);
    }

    #[test]
    fn projects_can_be_turned_off() {
        let c: ClaudeApiSync = toml::from_str("projects = false").unwrap();
        assert!(!c.projects);
    }

    /// Same derived-vs-serde trap as `projects`: the ceiling has to be
    /// on by default whichever way the struct is built.
    #[test]
    fn project_doc_ceiling_defaults_on_both_ways() {
        let from_toml: ClaudeRenderConfig = toml::from_str("").unwrap();
        assert_eq!(from_toml.max_project_doc_bytes, Some(128 * 1024));
        assert_eq!(
            ClaudeRenderConfig::default().max_project_doc_bytes,
            Some(128 * 1024)
        );
    }

    /// One store, two ways to fill it, and a config that names both
    /// would run a live download and then prune it to the export's
    /// snapshot. Refused at validate, with the alternative named.
    #[test]
    fn api_and_export_together_are_refused() {
        let c: ClaudeConfig =
            toml::from_str("api = {}\n[export]\npath = \"~/claude-export\"\n").unwrap();
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("both"), "{err}");
        let api_only: ClaudeConfig = toml::from_str("api = {}\n").unwrap();
        api_only.validate().unwrap();
        let export_only: ClaudeConfig =
            toml::from_str("[export]\npath = \"~/claude-export\"\n").unwrap();
        export_only.validate().unwrap();
        assert!(export_only
            .export
            .unwrap()
            .path()
            .ends_with("claude-export"));
    }

    #[test]
    fn project_uuids_default_to_empty_meaning_all() {
        let c: ClaudeApiSync = toml::from_str("").unwrap();
        assert!(c.project_uuids.is_empty());
        let c: ClaudeApiSync = toml::from_str(r#"project_uuids = ["a", "b"]"#).unwrap();
        assert_eq!(c.project_uuids, vec!["a".to_string(), "b".to_string()]);
    }
}
