//! Provider-owned config schema for the `codex` source: Codex CLI
//! sessions read off the store Codex itself keeps on this machine. The
//! shapes are the agent-session ones; this crate names them and says
//! where Codex keeps its home.

pub use datalib_etl_agent_sessions_config::{
    SessionsConfig as CodexConfig, SessionsRenderConfig as CodexRenderConfig,
    SessionsTable as CodexSessions,
};

/// This source's config stanzas, for `datalib-step help`.
pub const EXAMPLES: &[datalib_source_common::ConfigExample] = &[
    datalib_source_common::ConfigExample {
        kind: datalib_source_common::ExampleKind::Minimal,
        method: "sessions",
        toml: include_str!("../examples/minimal_sessions.toml"),
    },
    datalib_source_common::ConfigExample {
        kind: datalib_source_common::ExampleKind::Complete,
        method: "sessions",
        toml: include_str!("../examples/complete_sessions.toml"),
    },
];

/// The Codex home — what Codex calls `CODEX_HOME` — relative to `$HOME`.
pub const DEFAULT_CODEX_HOME: &str = "~/.codex";

/// The directories under the home that hold rollout files: `sessions/`,
/// and `archived_sessions/`, where an older Codex moved a thread on
/// archive.
pub const SESSION_DIRS: &[&str] = &["sessions", "archived_sessions"];
