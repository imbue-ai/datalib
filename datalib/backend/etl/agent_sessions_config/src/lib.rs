//! The config every agent-session source shares — claude_code and
//! codex, each reading the session store its agent keeps on this
//! machine. A provider's `<p>_config` crate names these under its own
//! names and says where its agent keeps its sessions.

use datalib_source_common::{expand_tilde, RenderCommon, SourceCommon};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// An agent-session source. `sessions` is its one way in today; a
/// `cloud` table for sessions run in the vendor's cloud is the planned
/// second (`docs/dev/plans/agent_sessions.md`).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionsConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    #[serde(default)]
    pub sessions: Option<SessionsTable>,
}

impl SessionsConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

impl datalib_source_common::IngestMethods for SessionsConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::local("sessions")];
}

/// The `sessions` table: the agent's own session store on this machine.
/// Every field is optional so that a bare `[steps.params.sessions]`
/// mirrors the standard location, the way an empty `[steps.params.gmail]`
/// mirrors the only stored account.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionsTable {
    /// Where the agent keeps its sessions; the provider's standard
    /// location when unset. Tilde-expanded on read.
    #[serde(default)]
    pub path: Option<PathBuf>,
}

impl SessionsTable {
    /// The configured path, else `standard`, tilde-expanded.
    pub fn path_or(&self, standard: &str) -> PathBuf {
        expand_tilde(self.path.as_deref().unwrap_or(Path::new(standard)))
    }
}

/// Params for the render step.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionsRenderConfig {
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

impl Default for SessionsRenderConfig {
    fn default() -> Self {
        Self {
            common: RenderCommon::default(),
            max_tool_result_bytes: default_max_tool_result_bytes(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An empty `sessions` table is the common case and must resolve to
    /// the standard location, not to the current directory.
    #[test]
    fn empty_sessions_table_means_the_standard_location() {
        let c: SessionsConfig = toml::from_str("[sessions]\n").unwrap();
        let p = c.sessions.unwrap().path_or("~/.agent/sessions");
        assert!(p.ends_with(".agent/sessions"), "{}", p.display());
        assert!(
            !p.starts_with("~"),
            "tilde must be expanded: {}",
            p.display()
        );
    }

    #[test]
    fn a_written_path_wins() {
        let c: SessionsConfig = toml::from_str("[sessions]\npath = \"/tmp/x\"\n").unwrap();
        assert_eq!(
            c.sessions.unwrap().path_or("~/.agent"),
            PathBuf::from("/tmp/x")
        );
    }

    /// The serde default and the `Default` impl have to agree, or a
    /// render step that omits the knob behaves differently from one
    /// built in Rust.
    #[test]
    fn render_default_agrees_both_ways() {
        let from_toml: SessionsRenderConfig = toml::from_str("").unwrap();
        assert_eq!(
            from_toml.max_tool_result_bytes,
            SessionsRenderConfig::default().max_tool_result_bytes
        );
    }
}
