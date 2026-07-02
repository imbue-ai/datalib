//! Schema-only config crate for the `hermes` source.
//!
//! Imports local-agent conversation transcripts from
//! [Hermes Agent](https://github.com/NousResearch) (and OpenClaw-compatible
//! runtimes that emit the same generic session/message shape).
//!
//! Two ways to point datalib at your history, mirroring the `chatgpt_api` /
//! `anthropic` split between managed sync and a manual export directory:
//!
//! * **Managed local import (primary UX)** — set `sync: {}`. datalib discovers
//!   the local agent history already on this machine (`$HOME/.hermes`,
//!   per-profile dirs, and OpenClaw-compatible roots) and reads each root's
//!   `state.db` read-only plus any legacy `sessions/*.json`. No export step, no
//!   network, no credentials; just like importing Claude/ChatGPT history.
//! * **Explicit export directory (advanced fallback)** — set
//!   `common.input_path` at a directory of exported `.jsonl` / `.json` session
//!   files. Useful when the history lives somewhere non-standard or was copied
//!   off another machine.
//!
//! Exactly one mode must be configured: either the `sync` block *or*
//! `common.input_path`, never both and never neither. Setting both is a
//! misconfiguration (they select different import sources) and fails loudly at
//! load rather than silently preferring one.
//!
//! Bazel-only by design (no Cargo.toml), mirroring `linkedin_config`.

use std::path::PathBuf;

use anyhow::Result;
use frankweiler_source_common::SourceCommon;
use serde::{Deserialize, Serialize};

/// Typed config for a `hermes` source.
///
/// Primary UX is `sync: {}` (managed local discovery/import); `common.input_path`
/// remains as an explicit export-directory fallback.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HermesConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    /// Managed local import/discovery. `sync: {}` (all-defaults) means "find the
    /// Hermes/OpenClaw agent history on this machine" — analogous to the
    /// `chatgpt_api` / `anthropic` sync modes.
    #[serde(default)]
    pub sync: Option<HermesSync>,
}

/// Managed local import knobs. Every field defaults, so `sync: {}` is valid and
/// means "default discovery of local agent history".
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HermesSync {
    /// Explicit roots to scan. When empty, datalib uses default discovery
    /// (`$HOME/.hermes`, its `profiles/*`, and OpenClaw-compatible roots when
    /// they exist). Tilde (`~`) is expanded.
    #[serde(default)]
    pub roots: Vec<PathBuf>,
    /// Include per-profile roots under `<root>/profiles/*` during default
    /// discovery. Defaults to `true` when unset.
    #[serde(default)]
    pub include_profiles: Option<bool>,
    /// Import legacy `<root>/sessions/*.json` files in addition to `state.db`.
    /// Defaults to `true` when unset.
    #[serde(default)]
    pub include_legacy_json_sessions: Option<bool>,
}

impl HermesSync {
    /// Whether to descend into `<root>/profiles/*` during default discovery.
    pub fn include_profiles(&self) -> bool {
        self.include_profiles.unwrap_or(true)
    }

    /// Whether to import legacy `<root>/sessions/*.json` alongside `state.db`.
    pub fn include_legacy_json_sessions(&self) -> bool {
        self.include_legacy_json_sessions.unwrap_or(true)
    }
}

impl HermesConfig {
    /// A `hermes` source must be told where to look, and told exactly once:
    /// either managed local discovery (`sync`) or an explicit export directory
    /// (`common.input_path`), but not both. Neither present is a
    /// misconfiguration (nothing to import); both present is also a
    /// misconfiguration (ambiguous — they select different sources). Either way
    /// we fail loudly at load instead of silently importing nothing or
    /// silently preferring one mode.
    pub fn validate(&self) -> Result<()> {
        let has_input = self
            .common
            .input_path
            .as_deref()
            .map(|p| !p.as_os_str().is_empty())
            .unwrap_or(false);
        let has_sync = self.sync.is_some();
        if !has_sync && !has_input {
            return Err(anyhow::anyhow!(
                "hermes source requires either `sync` (managed local import) or \
                 `common.input_path` (an explicit Hermes/OpenClaw export directory)"
            ));
        }
        if has_sync && has_input {
            return Err(anyhow::anyhow!(
                "hermes source sets both `sync` (managed local import) and \
                 `common.input_path` (an explicit export directory); these are \
                 mutually exclusive — configure exactly one mode"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_ok_with_sync_only() {
        let cfg = HermesConfig {
            sync: Some(HermesSync::default()),
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_ok_with_input_only() {
        let cfg = HermesConfig {
            common: SourceCommon {
                input_path: Some(PathBuf::from("/exports/hermes")),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_neither() {
        let cfg = HermesConfig::default();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_both_sync_and_input() {
        let cfg = HermesConfig {
            sync: Some(HermesSync::default()),
            common: SourceCommon {
                input_path: Some(PathBuf::from("/exports/hermes")),
                ..Default::default()
            },
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_rejects_empty_input_and_no_sync() {
        let cfg = HermesConfig {
            common: SourceCommon {
                input_path: Some(PathBuf::new()),
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn sync_defaults_are_permissive() {
        // `sync: {}` (all fields defaulted) discovers everything by default.
        let sync = HermesSync::default();
        assert!(sync.roots.is_empty());
        assert!(sync.include_profiles());
        assert!(sync.include_legacy_json_sessions());
    }
}
