//! Provider-owned config schema for the `fsindex` source (Program A goal #1).
//! Schema-only (serde + anyhow), so the orchestrator can name [`FsindexConfig`]
//! without linking the provider.
//!
//! `fsindex` is purely file-backed: there is no API and no `sync:` block — it's
//! a directory-tree scan driven by `input_path`. The only provider knob is
//! [`FsindexConfig::stamp`]; everything else (the scan root, the raw store
//! path) lives in the shared `common:` envelope.

use frankweiler_source_common::SourceCommon;
use serde::{Deserialize, Serialize};

/// The fsindex-owned slice of an `fsindex` source. The scan root is
/// `common.input_path`; `stamp` is the one knob.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct FsindexConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`. The scanned tree is `input_path`.
    #[serde(default)]
    pub common: SourceCommon,

    /// Write `.fsindex.yaml` UUID breadcrumbs into the scanned tree for any
    /// directory that opts in via `stamp_me_with_uuid`. **Off by default** for
    /// a config-driven scan, so the source stays read-only against its input —
    /// the only framework provider that can mutate its upstream stays opt-in.
    /// (The standalone `fsindex` CLI defaults stamping ON; this flag is the
    /// orchestrator-side inverse of its `--no-stamp`.)
    #[serde(default)]
    pub stamp: bool,
}

impl FsindexConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        Ok(())
    }
}
