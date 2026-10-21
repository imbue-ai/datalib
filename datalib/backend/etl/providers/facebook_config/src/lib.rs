//! Schema-only config crate for the `facebook` source: the serde
//! structs and nothing else, so anything that needs to understand a
//! config can link this without linking the ingest or render code.

use datalib_source_common::{LocalPath, SourceCommon};
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

/// The `facebook` slice of a source. `export` — the unpacked
/// "Download your information" export, in its JSON format — is its
/// one way in, and its only knob.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FacebookConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    #[serde(default)]
    pub export: Option<LocalPath>,
}

impl FacebookConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Params for the render step — no provider-specific render knobs, so
/// this is the shared bare envelope (see the per-phase params split).
pub type FacebookRenderConfig = datalib_source_common::BareRenderConfig;

impl datalib_source_common::IngestMethods for FacebookConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::local("export")];
}
