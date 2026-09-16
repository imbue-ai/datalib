//! Provider-owned config schema for the `claude_code` source: Claude Code
//! sessions read off the store Claude Code itself keeps on this machine.
//! Schema-only (serde + anyhow), so the orchestrator can name
//! `ClaudeCodeConfig` without linking the provider.

use datalib_source_common::{expand_tilde, RenderCommon, SourceCommon};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The claude_code-owned slice of a `claude_code` source. `sessions` is
/// its one way in today; a `cloud` table for sessions run on claude.ai
/// is the planned second (`docs/dev/plans/agent_sessions.md`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeCodeConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    #[serde(default)]
    pub sessions: Option<ClaudeCodeSessions>,
}

/// The `sessions` table: Claude Code's own session store on this
/// machine. Every field is optional so that a bare `[steps.params.sessions]`
/// mirrors the standard location, the way an empty `[steps.params.gmail]`
/// mirrors the only stored account.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeCodeSessions {
    /// The directory Claude Code writes transcripts under. Defaults to
    /// `~/.claude/projects`, which is where every Claude Code — terminal,
    /// desktop app, IDE extension — keeps them. Tilde-expanded on read.
    #[serde(default)]
    pub path: Option<PathBuf>,
}

/// Where Claude Code keeps its transcripts, relative to `$HOME`.
pub const DEFAULT_SESSIONS_DIR: &str = "~/.claude/projects";

impl ClaudeCodeSessions {
    pub fn path(&self) -> PathBuf {
        match &self.path {
            Some(p) => expand_tilde(p),
            None => expand_tilde(std::path::Path::new(DEFAULT_SESSIONS_DIR)),
        }
    }
}

impl ClaudeCodeConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Params for the render step.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClaudeCodeRenderConfig {
    #[serde(default)]
    pub common: RenderCommon,
    /// How much of one tool result reaches the page. A tool result can be
    /// a whole file echoed back; the raw store keeps all of it and the
    /// rendered document keeps this much, with a marker saying what was
    /// cut. Raising it and re-rendering backfills.
    #[serde(default = "default_max_tool_result_bytes")]
    pub max_tool_result_bytes: usize,
}

fn default_max_tool_result_bytes() -> usize {
    16 * 1024
}

impl Default for ClaudeCodeRenderConfig {
    fn default() -> Self {
        Self {
            common: RenderCommon::default(),
            max_tool_result_bytes: default_max_tool_result_bytes(),
        }
    }
}

impl datalib_source_common::IngestMethods for ClaudeCodeConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::local("sessions")];
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty `sessions` table is the common case and must resolve to
    /// the standard store, not to the current directory.
    #[test]
    fn empty_sessions_table_means_the_standard_store() {
        let c: ClaudeCodeConfig = toml::from_str("[sessions]\n").unwrap();
        let p = c.sessions.unwrap().path();
        assert!(p.ends_with(".claude/projects"), "{}", p.display());
        assert!(
            !p.starts_with("~"),
            "tilde must be expanded: {}",
            p.display()
        );
    }

    #[test]
    fn a_written_path_wins() {
        let c: ClaudeCodeConfig = toml::from_str("[sessions]\npath = \"/tmp/x\"\n").unwrap();
        assert_eq!(c.sessions.unwrap().path(), PathBuf::from("/tmp/x"));
    }

    /// The serde default and the `Default` impl have to agree, or a
    /// render step that omits the knob behaves differently from one
    /// built in Rust.
    #[test]
    fn render_default_agrees_both_ways() {
        let from_toml: ClaudeCodeRenderConfig = toml::from_str("").unwrap();
        assert_eq!(
            from_toml.max_tool_result_bytes,
            ClaudeCodeRenderConfig::default().max_tool_result_bytes
        );
    }
}
