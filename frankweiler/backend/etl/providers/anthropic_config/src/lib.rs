//! Provider-owned config schema for the `claude_api` / `claude_export` sources
//! (Program A goal #1). Schema-only (serde + anyhow), so the orchestrator and
//! `http` can name `AnthropicConfig` without linking the provider.

use frankweiler_source_common::SourceCommon;
use serde::{Deserialize, Serialize};

/// The anthropic-owned slice of a `claude_api` (or `claude_export`) source.
/// `sync:` present → live Claude.ai mirror (the extract path); absent →
/// translate-only over an already-on-disk export (`claude_export`).
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
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
}
