//! Provider-owned config schema for the `notion_api` source (Program A
//! goal #1). Schema-only (serde + anyhow), so the orchestrator and `http`
//! can name `NotionConfig` without linking the provider.

use frankweiler_source_common::SourceCommon;
use serde::{Deserialize, Serialize};

/// The notion-owned slice of a `notion_api` source. `sync:` present → live
/// Notion mirror (the extract path); absent → translate-only over an
/// already-on-disk API capture.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NotionConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    #[serde(default)]
    pub sync: Option<NotionApiSync>,
}

impl NotionConfig {
    /// Per-source sync constraint (moved out of `core::config::Config::validate`):
    /// when `sync:` is present it must enable the inbox or list at least one
    /// subtree page, else there is nothing to seed the BFS with.
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(sync) = &self.sync {
            let inbox_on = sync.inbox.as_ref().is_some_and(|i| i.enabled);
            let subtrees_on = sync.subtrees.as_ref().is_some_and(|t| !t.pages.is_empty());
            if !inbox_on && !subtrees_on {
                return Err(anyhow::anyhow!(
                    "notion_api source sync: must enable inbox or list at least one \
                     subtree page"
                ));
            }
        }
        Ok(())
    }
}

/// Notion sync knobs (inbox discovery + explicit subtree seeds).
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct NotionApiSync {
    #[serde(default)]
    pub refresh_window_days: Option<i64>,
    #[serde(default)]
    pub inbox: Option<NotionInbox>,
    #[serde(default)]
    pub subtrees: Option<NotionSubtrees>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NotionInbox {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub types: Option<Vec<String>>,
    #[serde(default)]
    pub notification_page_size: Option<i64>,
    #[serde(default)]
    pub max_notification_pages: Option<i64>,
    #[serde(default)]
    pub space: Option<String>,
    /// When `false`, walk the inbox to discover referenced page IDs (and
    /// log them) but don't BFS into them. Useful for keeping the inbox
    /// signal without dragging hundreds of unrelated pages through the
    /// mirror. Defaults to `true` for back-compat.
    #[serde(default)]
    pub mirror_referenced_pages: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct NotionSubtrees {
    /// Page IDs at the root of each subtree to walk. Accepts bare page
    /// IDs (dashed or undashed) or paste-able browser URLs
    /// (`https://www.notion.so/<workspace>/<title>-<hex32>`); URLs are
    /// reduced to the trailing 32-hex token before being passed through
    /// `format_uuid` in the notion extractor.
    #[serde(default)]
    pub pages: Vec<String>,
    #[serde(default)]
    pub max_pages: Option<i64>,
}

fn default_true() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_only_config_validates() {
        // No `sync:` → translate-only; the inbox-or-subtree rule doesn't apply.
        assert!(NotionConfig::default().validate().is_ok());
    }

    #[test]
    fn sync_with_inbox_enabled_validates() {
        let cfg = NotionConfig {
            common: Default::default(),
            sync: Some(NotionApiSync {
                inbox: Some(NotionInbox {
                    enabled: true,
                    types: None,
                    notification_page_size: None,
                    max_notification_pages: None,
                    space: None,
                    mirror_referenced_pages: None,
                }),
                ..Default::default()
            }),
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn sync_with_subtree_pages_validates() {
        let cfg = NotionConfig {
            common: Default::default(),
            sync: Some(NotionApiSync {
                subtrees: Some(NotionSubtrees {
                    pages: vec!["abc123".into()],
                    max_pages: None,
                }),
                ..Default::default()
            }),
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn sync_without_inbox_or_subtrees_is_rejected() {
        let cfg = NotionConfig {
            common: Default::default(),
            sync: Some(NotionApiSync::default()),
        };
        let err = cfg.validate().unwrap_err();
        assert!(err
            .to_string()
            .contains("must enable inbox or list at least one"));
    }

    #[test]
    fn sync_with_inbox_disabled_and_empty_subtrees_is_rejected() {
        let cfg = NotionConfig {
            common: Default::default(),
            sync: Some(NotionApiSync {
                inbox: Some(NotionInbox {
                    enabled: false,
                    types: None,
                    notification_page_size: None,
                    max_notification_pages: None,
                    space: None,
                    mirror_referenced_pages: None,
                }),
                subtrees: Some(NotionSubtrees::default()),
                ..Default::default()
            }),
        };
        assert!(cfg.validate().is_err());
    }
}
