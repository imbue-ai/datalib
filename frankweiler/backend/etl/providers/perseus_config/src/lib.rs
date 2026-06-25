//! Provider-owned config schema for the `perseus` source (Program A goal #1).
//! Schema-only (serde + anyhow).

use frankweiler_source_common::SourceCommon;
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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PerseusSync {
    /// TEI subpaths to fetch from PerseusDL/canonical-greekLit.
    #[serde(default)]
    pub files: Vec<String>,
    /// Edition pairs to sentence-align within each section, as
    /// `[edition_a, edition_b]`. Empty = no alignment (the fast default).
    #[serde(default)]
    pub alignment_pairs: Vec<[String; 2]>,
}
