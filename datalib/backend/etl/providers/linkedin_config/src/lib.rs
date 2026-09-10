//! Schema-only config crate for the `linkedin` source (Program A goal #1).

use anyhow::Result;
use std::path::PathBuf;

use datalib_source_common::SourceCommon;
use serde::{Deserialize, Serialize};

/// Typed config for a `linkedin` source: `export`, the data export on
/// disk, whose `fetch_photos` is the one thing that reaches linkedin.com.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkedinConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    #[serde(default)]
    pub export: Option<LinkedinExport>,
}

/// The `export` table: where the unpacked export is, and whether to
/// fetch each connection's public profile photo while ingesting it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkedinExport {
    /// The unpacked LinkedIn data export: the directory of CSVs.
    pub path: PathBuf,
    #[serde(default)]
    pub fetch_photos: bool,
}

impl LinkedinExport {
    pub fn path(&self) -> PathBuf {
        datalib_source_common::expand_tilde(&self.path)
    }
}

impl LinkedinConfig {
    pub fn validate(&self) -> Result<()> {
        Ok(())
    }
}

/// Params for the render step — no provider-specific render knobs, so
/// this is the shared bare envelope (see the per-phase params split).
pub type LinkedinRenderConfig = datalib_source_common::BareRenderConfig;

// The export is read off disk; its `fetch_photos` alone reaches
// linkedin.com, so a step with it on reads "Download".
impl datalib_source_common::IngestMethods for LinkedinConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] = &[
        datalib_source_common::IngestMethod::local("export"),
        datalib_source_common::IngestMethod::origin("export.fetch_photos"),
    ];
}
