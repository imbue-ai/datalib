//! Schema-only config crate for the `linkedin` source (Program A goal #1).

use anyhow::Result;
use std::path::PathBuf;

use datalib_source_common::SourceCommon;
use serde::{Deserialize, Serialize};

/// This source's config stanzas, for `datalib-step help`.
pub const EXAMPLES: &[datalib_source_common::ConfigExample] = &[
    datalib_source_common::ConfigExample {
        kind: datalib_source_common::ExampleKind::Minimal,
        method: "export",
        toml: include_str!("../examples/minimal_export.toml"),
    },
    datalib_source_common::ConfigExample {
        kind: datalib_source_common::ExampleKind::Complete,
        method: "export",
        toml: include_str!("../examples/complete_export.toml"),
    },
];

/// Typed config for a `linkedin` source: `export`, the data export on
/// disk.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LinkedinConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    #[serde(default)]
    pub export: Option<LinkedinExport>,
}

/// The `export` table: where the unpacked export is.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct LinkedinExport {
    /// The unpacked LinkedIn data export: the directory of CSVs.
    pub path: PathBuf,
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

impl datalib_source_common::IngestMethods for LinkedinConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::local("export")];
}
