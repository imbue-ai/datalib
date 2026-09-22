//! Provider-owned config schema for the `codex` source: Codex CLI
//! sessions read off the store Codex itself keeps on this machine.
//! Schema-only (serde + anyhow), so the orchestrator can name
//! `CodexConfig` without linking the provider.

use datalib_source_common::{expand_tilde, RenderCommon, SourceCommon};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// The codex-owned slice of a `codex` source. `sessions` is its one way
/// in today; a `cloud` table for tasks run on Codex cloud is the planned
/// second (`docs/dev/plans/agent_sessions.md`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodexConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    #[serde(default)]
    pub sessions: Option<CodexSessions>,
}

/// The `sessions` table: Codex's own home directory on this machine.
/// Every field is optional so that a bare `[steps.params.sessions]`
/// mirrors the standard location, the way an empty `[steps.params.gmail]`
/// mirrors the only stored account.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodexSessions {
    /// The Codex home — what Codex calls `CODEX_HOME` — holding
    /// `sessions/` and, when Codex has archived a thread the old way,
    /// `archived_sessions/`. Defaults to `~/.codex`. Tilde-expanded on
    /// read.
    #[serde(default)]
    pub path: Option<PathBuf>,
}

/// Where Codex keeps its home, relative to `$HOME`.
pub const DEFAULT_CODEX_HOME: &str = "~/.codex";

/// The directories under the home that hold rollout files.
pub const SESSION_DIRS: &[&str] = &["sessions", "archived_sessions"];

impl CodexSessions {
    pub fn path(&self) -> PathBuf {
        match &self.path {
            Some(p) => expand_tilde(p),
            None => expand_tilde(std::path::Path::new(DEFAULT_CODEX_HOME)),
        }
    }
}

impl CodexConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Params for the render step.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CodexRenderConfig {
    #[serde(default)]
    pub common: RenderCommon,
    /// How much of one tool output reaches the page. A command's output
    /// can be a whole file echoed back; the raw store keeps all of it
    /// and the rendered document keeps this much, with a marker saying
    /// what was cut. Raising it and re-rendering backfills.
    #[serde(default = "default_max_tool_result_bytes")]
    pub max_tool_result_bytes: usize,
}

fn default_max_tool_result_bytes() -> usize {
    16 * 1024
}

impl Default for CodexRenderConfig {
    fn default() -> Self {
        Self {
            common: RenderCommon::default(),
            max_tool_result_bytes: default_max_tool_result_bytes(),
        }
    }
}

impl datalib_source_common::IngestMethods for CodexConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::local("sessions")];
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty `sessions` table is the common case and must resolve to
    /// the standard home, not to the current directory.
    #[test]
    fn empty_sessions_table_means_the_standard_home() {
        let c: CodexConfig = toml::from_str("[sessions]\n").unwrap();
        let p = c.sessions.unwrap().path();
        assert!(p.ends_with(".codex"), "{}", p.display());
        assert!(
            !p.starts_with("~"),
            "tilde must be expanded: {}",
            p.display()
        );
    }

    #[test]
    fn a_written_path_wins() {
        let c: CodexConfig = toml::from_str("[sessions]\npath = \"/tmp/x\"\n").unwrap();
        assert_eq!(c.sessions.unwrap().path(), PathBuf::from("/tmp/x"));
    }

    /// The serde default and the `Default` impl have to agree, or a
    /// render step that omits the knob behaves differently from one
    /// built in Rust.
    #[test]
    fn render_default_agrees_both_ways() {
        let from_toml: CodexRenderConfig = toml::from_str("").unwrap();
        assert_eq!(
            from_toml.max_tool_result_bytes,
            CodexRenderConfig::default().max_tool_result_bytes
        );
    }
}
