//! Provider-owned config schema for the `perseus` source (Program A goal #1).
//! Schema-only (serde + anyhow).

use frankweiler_source_common::{RenderCommon, SourceCommon};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PerseusConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    #[serde(default)]
    pub sync: Option<PerseusSync>,
}

impl PerseusConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Params for the perseus **render** step. Split from [`PerseusConfig`]
/// (the download-step params) so each step's params carry only what
/// that wave reads. Perseus is file-tree-backed, so render reads
/// `common.input_path` (else the raw dir) directly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerseusRenderConfig {
    #[serde(default)]
    pub common: RenderCommon,
    /// Edition pairs to sentence-align within each section, as
    /// `[edition_a, edition_b]`. Empty = no alignment (the fast default).
    #[serde(default)]
    pub alignment_pairs: Vec<[String; 2]>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerseusSync {
    /// TEI subpaths to fetch from PerseusDL/canonical-greekLit.
    #[serde(default)]
    pub files: Vec<String>,
    /// Legacy location of the alignment pairs — the knob now lives on
    /// the render step's params ([`PerseusRenderConfig::alignment_pairs`]).
    /// Still parsed here so old-format configs migrate losslessly; the
    /// download planner rejects it with a pointer to the new home.
    #[serde(default)]
    pub alignment_pairs: Vec<[String; 2]>,
}
