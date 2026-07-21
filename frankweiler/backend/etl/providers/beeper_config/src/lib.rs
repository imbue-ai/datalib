//! Provider-owned config schema for the `beeper` source (Program A goal #1).
//! Schema-only (serde + anyhow), so the orchestrator can name `BeeperConfig`
//! without linking the provider.

use std::path::PathBuf;

use frankweiler_source_common::{RenderCommon, SourceCommon};
use serde::{Deserialize, Serialize};

/// The beeper-owned slice of a `beeper` source. `sync:` present → live
/// Beeper Texts ingest (the download path); absent → render-only over an
/// already-on-disk raw store.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BeeperConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    #[serde(default)]
    pub sync: Option<BeeperSync>,
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
}

/// Beeper Texts ingest knobs (which networks, where, media, period).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BeeperSync {
    /// Canonical chat network names to ingest (`"signal"`,
    /// `"googlechat"`, future: `"slack"`, `"whatsapp"`, …). Empty
    /// list is an error at fetch time — caller should pick at least
    /// one explicitly.
    #[serde(default)]
    pub sources: Vec<String>,
    /// Override for Beeper Texts' data dir. Defaults to
    /// `~/Library/Application Support/BeeperTexts` on macOS.
    #[serde(default)]
    pub beeper_data_dir: Option<PathBuf>,
    /// Copy cached media bytes into the `blobs` table. Off = metadata
    /// + source URL only.
    #[serde(default = "default_true")]
    pub media: bool,
    /// Legacy location of the render period — the knob now lives on
    /// the render step's params ([`BeeperRenderConfig::period`]).
    /// Still parsed here so old-format configs migrate losslessly;
    /// the download planner rejects it with a pointer to the new home.
    #[serde(default)]
    pub period: Option<String>,
}

fn default_true() -> bool {
    true
}
