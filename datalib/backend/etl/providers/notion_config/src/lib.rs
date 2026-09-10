//! Provider-owned config schema for the `notion_api` source. Schema-only
//! (serde + anyhow), so the orchestrator and `http` can name
//! `NotionConfig` without linking the provider.

use datalib_source_common::{LatchkeySettings, SourceCommon};
use serde::{Deserialize, Serialize};

/// The notion-owned slice of a `notion_api` source. `sync` is its one
/// way in; an `ingest` step without it is refused (`IngestMethods` below).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NotionConfig {
    #[serde(default)]
    pub common: SourceCommon,
    /// Which latchkey identity this source mirrors. The stored
    /// credential must carry both the bearer token and the
    /// `Notion-Version` header: the two cannot be split, because a
    /// second `Notion-Version` on the wire concatenates with the stored
    /// one and Notion rejects the pair.
    #[serde(default)]
    pub latchkey_settings: LatchkeySettings,
    #[serde(default)]
    pub sync: Option<NotionSync>,
}

impl NotionConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        self.latchkey_settings
            .validate()
            .map_err(anyhow::Error::msg)?;
        if let Some(sync) = &self.sync {
            sync.validate()?;
        }
        Ok(())
    }
}

/// Notion sync knobs.
///
/// There is deliberately no "you must name a starting point" rule. A
/// personal access token sees what its creator sees, and `POST
/// /v1/search` enumerates that, so the useful default is the whole
/// workspace. `roots` narrows it; it never enables it.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct NotionSync {
    /// Re-examine anything edited within this many days even when the
    /// stored resume cursor is newer. Zero (or absent) means no floor.
    #[serde(default)]
    pub refresh_window_days: Option<u32>,
    /// Optional allowlist. Each entry is a page id or a paste-able
    /// browser URL; the mirror is then that page plus everything under
    /// it (child pages, and the rows of databases embedded in it).
    /// Empty means the whole workspace.
    #[serde(default)]
    pub roots: Vec<String>,
    /// Stop after this many pages. A guard against a mis-scoped run,
    /// not a tuning knob.
    #[serde(default)]
    pub max_pages: Option<u32>,
    #[serde(default = "default_true")]
    pub comments: bool,
    #[serde(default = "default_true")]
    pub attachments: bool,
    /// Walk databases found under a root and mirror their rows.
    #[serde(default = "default_true")]
    pub databases: bool,
}

impl NotionSync {
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(0) = self.max_pages {
            return Err(anyhow::anyhow!(
                "notion_api sync.max_pages = 0 mirrors nothing; omit it for no limit"
            ));
        }
        Ok(())
    }
}

fn default_true() -> bool {
    true
}

/// Params for the render step — no provider-specific render knobs, so
/// this is the shared bare envelope (see the per-phase params split).
pub type NotionRenderConfig = datalib_source_common::BareRenderConfig;

impl datalib_source_common::IngestMethods for NotionConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::origin("sync")];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_only_config_validates() {
        assert!(NotionConfig::default().validate().is_ok());
    }

    /// The rule this replaces: the old schema *rejected* a sync block
    /// that named neither an inbox nor a subtree page, because there
    /// was no way to discover pages without a seed. Search removed that
    /// constraint, so an empty sync block is now the whole-workspace
    /// mirror and must validate.
    #[test]
    fn empty_sync_means_whole_workspace_and_validates() {
        let cfg = NotionConfig {
            sync: Some(NotionSync::default()),
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
        assert!(cfg.sync.unwrap().roots.is_empty());
    }

    #[test]
    fn roots_narrow_the_mirror() {
        let cfg: NotionConfig = serde_json::from_str(
            r#"{"sync":{"roots":["https://app.notion.com/p/Proj-348a550faf9580a08973e679d9e1c6c9"]}}"#,
        )
        .unwrap();
        assert!(cfg.validate().is_ok());
        assert_eq!(cfg.sync.unwrap().roots.len(), 1);
    }

    /// The three include-toggles default ON, so a bare `sync = {}`
    /// mirrors everything rather than quietly mirroring only bodies.
    #[test]
    fn include_toggles_default_on() {
        let s: NotionSync = serde_json::from_str("{}").unwrap();
        assert!(s.comments && s.attachments && s.databases);
    }

    #[test]
    fn zero_max_pages_is_rejected() {
        let cfg = NotionConfig {
            sync: Some(NotionSync {
                max_pages: Some(0),
                ..Default::default()
            }),
            ..Default::default()
        };
        assert!(cfg
            .validate()
            .unwrap_err()
            .to_string()
            .contains("mirrors nothing"));
    }
}
