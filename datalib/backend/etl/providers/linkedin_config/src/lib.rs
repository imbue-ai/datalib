//! Schema-only config crate for the `linkedin` source (Program A goal #1).

use anyhow::Result;
use datalib_source_common::SourceCommon;
use serde::{Deserialize, Serialize};

/// Typed config for a `linkedin` source. `fetch_photos` lives at the top
/// level (LinkedIn has no managed `sync:` block).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LinkedinConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    /// Whether to download connection profile photos during download.
    #[serde(default)]
    pub fetch_photos: bool,
}

impl LinkedinConfig {
    pub fn validate(&self) -> Result<()> {
        Ok(())
    }
}

/// Params for the render step — no provider-specific render knobs, so
/// this is the shared bare envelope (see the per-phase params split).
pub type LinkedinRenderConfig = datalib_source_common::BareRenderConfig;

// The export is read off disk; `fetch_photos` alone reaches linkedin.com,
// so a step with it on reads "Download".
impl datalib_source_common::IngestMethods for LinkedinConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] = &[
        datalib_source_common::IngestMethod::local("common.input_path"),
        datalib_source_common::IngestMethod::origin("fetch_photos"),
    ];
}
