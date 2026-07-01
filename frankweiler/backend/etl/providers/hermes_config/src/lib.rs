//! Schema-only config crate for the `hermes` source.
//!
//! Imports local-agent conversation transcripts exported from
//! [Hermes Agent](https://github.com/NousResearch) (and OpenClaw-compatible
//! runtimes that emit the same generic session/message shape) from a directory
//! on disk. File-backed and translate-only: no network, no live `state.db`, no
//! credentials — [`HermesConfig`] just points `common.input_path` at an export
//! directory of `.jsonl` / `.json` session files.
//!
//! Bazel-only by design (no Cargo.toml), mirroring `linkedin_config`.

use anyhow::Result;
use frankweiler_source_common::SourceCommon;
use serde::{Deserialize, Serialize};

/// Typed config for a `hermes` source. File-backed only — the export root is
/// `common.input_path`; there is no managed `sync:` block.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HermesConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
}

impl HermesConfig {
    /// A `hermes` source is file-backed: it must carry an `input_path` pointing
    /// at the export directory. (The orchestrator only marks it managed when
    /// `input_path` is set; this makes a misconfigured stanza fail loudly at
    /// load instead of silently doing nothing.)
    pub fn validate(&self) -> Result<()> {
        let missing = self
            .common
            .input_path
            .as_deref()
            .map(|p| p.as_os_str().is_empty())
            .unwrap_or(true);
        if missing {
            return Err(anyhow::anyhow!(
                "hermes source requires common.input_path (the Hermes/OpenClaw export directory)"
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn validate_requires_input_path() {
        let cfg = HermesConfig::default();
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn validate_ok_with_input_path() {
        let cfg = HermesConfig {
            common: SourceCommon {
                input_path: Some(PathBuf::from("/exports/hermes")),
                ..Default::default()
            },
        };
        assert!(cfg.validate().is_ok());
    }

    #[test]
    fn validate_rejects_empty_input_path() {
        let cfg = HermesConfig {
            common: SourceCommon {
                input_path: Some(PathBuf::new()),
                ..Default::default()
            },
        };
        assert!(cfg.validate().is_err());
    }
}
