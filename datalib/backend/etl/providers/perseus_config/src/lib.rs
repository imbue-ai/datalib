//! Provider-owned config schema for the `perseus` source (Program A goal #1).
//! Schema-only (serde + anyhow).

use datalib_source_common::{RenderCommon, SourceCommon};
use serde::{Deserialize, Serialize};

/// This source's config stanzas, for `datalib-step help`.
pub const EXAMPLES: &[datalib_source_common::ConfigExample] = &[
    datalib_source_common::ConfigExample {
        kind: datalib_source_common::ExampleKind::Minimal,
        method: "github",
        toml: include_str!("../examples/minimal_github.toml"),
    },
    datalib_source_common::ConfigExample {
        kind: datalib_source_common::ExampleKind::Complete,
        method: "github",
        toml: include_str!("../examples/complete_github.toml"),
    },
];

/// The perseus-owned slice of a `perseus` source. `github` — TEI files
/// fetched from PerseusDL — is its one ingest method. A tree staged by
/// hand has no ingest step at all: its render step names the tree on
/// `common.input_path`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PerseusConfig {
    /// Shared per-source envelope (paths + cross-source tunables), resolved by
    /// the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,
    #[serde(default)]
    pub github: Option<PerseusSync>,
}

impl PerseusConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// Params for the perseus **render** step. Split from [`PerseusConfig`]
/// (the download-step params) so each step's params carry only what
/// that wave reads. Perseus is file-tree-backed, so render reads
/// `common.input_path` (a tree staged by hand) else the ingest tree.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PerseusRenderConfig {
    #[serde(default)]
    pub common: RenderCommon,
    /// Edition pairs to sentence-align within each section, as
    /// `[edition_a, edition_b]`. Empty = no alignment (the fast default).
    #[serde(default)]
    pub alignment_pairs: Vec<[String; 2]>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PerseusSync {
    /// TEI files to fetch, as paths inside PerseusDL/canonical-greekLit's
    /// `data/` folder. Empty fetches the default Thucydides editions.
    #[serde(default)]
    pub files: Vec<String>,
    /// Where `alignment_pairs` used to go. It now belongs on the render
    /// step's params; an older config that still sets it here loads, and
    /// the ingest step refuses it with a pointer to the render step. Out
    /// of the schema, so `datalib-step help` does not offer it.
    #[serde(default)]
    #[schemars(skip)]
    pub alignment_pairs: Vec<[String; 2]>,
}

impl datalib_source_common::IngestMethods for PerseusConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::origin("github")];
}
