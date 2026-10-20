//! Provider-owned config schema for the `beeper` source (Program A goal #1).
//! Schema-only (serde + anyhow), so the orchestrator can name `BeeperConfig`
//! without linking the provider.

use std::path::PathBuf;

use datalib_source_common::{RenderCommon, SourceCommon};
use serde::{Deserialize, Serialize};

/// The beeper-owned slice of a `beeper` source. `texts` — the Beeper
/// Texts desktop app's own database — is its one way in; an `ingest`
/// step without it is refused (`IngestMethods` below).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeeperConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    #[serde(default)]
    pub texts: Option<BeeperSync>,
}

impl BeeperConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Params for the beeper **render** step. Split from [`BeeperConfig`]
/// (the download-step params) so each step's params carry only what
/// that wave reads.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeeperRenderConfig {
    #[serde(default)]
    pub common: RenderCommon,
    /// Period each rendered markdown document covers. One of
    /// `"month"` (default), `"day"`, `"year"`, or `"all"` (single
    /// file per conversation). Reactions render in the period of
    /// the message they target, regardless of when the reaction
    /// itself landed.
    #[serde(default)]
    pub period: Option<String>,
    /// Canonical chat network names to render (`"signal"`,
    /// `"googlechat"`, `"slack"`, …). Unset renders every network the
    /// mirror holds; the download mirrors them all either way.
    #[serde(default)]
    pub networks: Option<Vec<String>>,
}

/// The `texts` table: where Beeper Texts' data directory is, and whether
/// to copy its cached media.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeeperSync {
    /// Beeper Texts' data directory. Defaults to
    /// `~/Library/Application Support/BeeperTexts` on macOS.
    #[serde(default)]
    pub path: Option<PathBuf>,
    /// Copy the app's cached media files into the blob store. Off = the
    /// messages still name their attachments, with no bytes.
    #[serde(default = "default_true")]
    pub media: bool,
}

fn default_true() -> bool {
    true
}

impl BeeperSync {
    pub fn path(&self) -> Option<PathBuf> {
        self.path
            .as_deref()
            .map(datalib_source_common::expand_tilde)
    }
}

// Beeper Texts' own SQLite, read off this machine: nothing reaches a
// service.
impl datalib_source_common::IngestMethods for BeeperConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::local("texts")];
}
