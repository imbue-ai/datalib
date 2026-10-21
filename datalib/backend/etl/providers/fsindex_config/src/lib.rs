//! Provider-owned config schema for the `fsindex` source (Program A goal #1).
//! Schema-only (serde + anyhow), so the orchestrator can name [`FsindexConfig`]
//! without linking the provider.

use datalib_source_common::{LocalPath, SourceCommon};
use serde::{Deserialize, Serialize};

/// This source's config stanzas, for `datalib-step help`.
pub const EXAMPLES: &[datalib_source_common::ConfigExample] = &[
    datalib_source_common::ConfigExample {
        kind: datalib_source_common::ExampleKind::Minimal,
        method: "fswalk",
        toml: include_str!("../examples/minimal_fswalk.toml"),
    },
    datalib_source_common::ConfigExample {
        kind: datalib_source_common::ExampleKind::Complete,
        method: "fswalk",
        toml: include_str!("../examples/complete_fswalk.toml"),
    },
];

/// The fsindex-owned slice of an `fsindex` source. The scan root is
/// `fswalk.path`; `stamp` is the one knob.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct FsindexConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,

    /// The tree to scan.
    #[serde(default)]
    pub fswalk: Option<LocalPath>,

    /// Write `.fsindex.toml` UUID breadcrumbs into the scanned tree for any
    /// directory that opts in via `stamp_me_with_uuid`. Off by default, so
    /// the scan only reads the tree. (The standalone `fsindex` command
    /// stamps unless given `--no-stamp`.)
    #[serde(default)]
    pub stamp: bool,
}

impl FsindexConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Params for the render step — no provider-specific render knobs, so
/// this is the shared bare envelope (see the per-phase params split).
pub type FsindexRenderConfig = datalib_source_common::BareRenderConfig;

impl datalib_source_common::IngestMethods for FsindexConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::local("fswalk")];
}
