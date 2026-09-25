//! Provider-owned config schema for the `claude_code` source: Claude Code
//! sessions read off the store Claude Code itself keeps on this machine.
//! The shapes are the agent-session ones; this crate names them and
//! says where Claude Code keeps its transcripts.

pub use datalib_etl_agent_sessions_config::{
    SessionsConfig as ClaudeCodeConfig, SessionsRenderConfig as ClaudeCodeRenderConfig,
    SessionsTable as ClaudeCodeSessions,
};

/// Where Claude Code writes transcripts, relative to `$HOME`: every
/// Claude Code — terminal, desktop app, IDE extension — keeps them here.
pub const DEFAULT_SESSIONS_DIR: &str = "~/.claude/projects";
