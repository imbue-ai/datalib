//! Provider-owned config schema for the `claude_api` / `claude_export` sources
//! (Program A goal #1). Schema-only (serde + anyhow), so the orchestrator and
//! `http` can name `AnthropicConfig` without linking the provider.

use datalib_source_common::SourceCommon;
use serde::{Deserialize, Serialize};

/// The anthropic-owned slice of a `claude_api` (or `claude_export`) source.
/// `sync:` present → live Claude.ai mirror (the download path); absent →
/// render-only over an already-on-disk export (`claude_export`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AnthropicConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    #[serde(default)]
    pub sync: Option<ClaudeApiSync>,
}

impl AnthropicConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// JMAP-less Claude.ai sync knobs (conversation refresh + explicit UUIDs).
///
/// `Default` is hand-written rather than derived so it agrees with the
/// serde defaults: `projects` defaults to `true`, and a derived
/// `Default` would silently make it `false` for any caller that builds
/// the struct in Rust instead of deserializing it.
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
    ///
    /// Applies in `conv_uuids` mode too. That mode skips the
    /// *conversation* listing walk, not this one: a targeted chat still
    /// resolves its `project` grid column against the mirrored
    /// projects, and would otherwise show a bare UUID. Set this to
    /// `false` to opt out.
    #[serde(default = "default_true")]
    pub projects: bool,
    /// When non-empty, restrict the project mirror to exactly these
    /// project UUIDs (bare UUID or a paste-able
    /// `https://claude.ai/project/<uuid>` URL). The per-org listing
    /// still runs — it is one request and it is where the metadata
    /// comes from — but every project outside this set is left alone.
    ///
    /// Intended for development and for bounding a first run against a
    /// large account; leave it empty to mirror everything. Independent
    /// of `conv_uuids`, which scopes conversations only.
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
pub struct AnthropicRenderConfig {
    #[serde(default)]
    pub common: datalib_source_common::RenderCommon,

    /// Truncate a project knowledge document's inline text at this many
    /// bytes when rendering it into the project's page.
    ///
    /// Claude extracts text from *any* uploaded knowledge file, so a
    /// project whose "document" is a 500-page EPUB yields half a
    /// megabyte of pandoc-flavored markup in `content` — which would
    /// otherwise become a half-megabyte markdown page and a single
    /// `grid_rows.text` cell of the same size. Hand-written project
    /// knowledge is a few KB; this ceiling is far above that and far
    /// below a book.
    ///
    /// Truncation is a *render* concern only: the raw store keeps the
    /// full text either way, so raising this and re-rendering
    /// backfills. `None` disables the ceiling.
    #[serde(default = "default_max_project_doc_bytes")]
    pub max_project_doc_bytes: Option<usize>,
}

/// 128 KiB.
fn default_max_project_doc_bytes() -> Option<usize> {
    Some(128 * 1024)
}

impl Default for AnthropicRenderConfig {
    fn default() -> Self {
        Self {
            common: Default::default(),
            max_project_doc_bytes: default_max_project_doc_bytes(),
        }
    }
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
        let from_toml: AnthropicRenderConfig = toml::from_str("").unwrap();
        assert_eq!(from_toml.max_project_doc_bytes, Some(128 * 1024));
        assert_eq!(
            AnthropicRenderConfig::default().max_project_doc_bytes,
            Some(128 * 1024)
        );
    }

    #[test]
    fn project_uuids_default_to_empty_meaning_all() {
        let c: ClaudeApiSync = toml::from_str("").unwrap();
        assert!(c.project_uuids.is_empty());
        let c: ClaudeApiSync = toml::from_str(r#"project_uuids = ["a", "b"]"#).unwrap();
        assert_eq!(c.project_uuids, vec!["a".to_string(), "b".to_string()]);
    }
}
