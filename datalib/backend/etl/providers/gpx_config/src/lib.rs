//! Provider-owned config schema for the `gpx` source. Schema-only
//! (serde + anyhow), so the orchestrator can name [`GpxConfig`] without
//! linking the provider.

use datalib_source_common::{LocalPath, SourceCommon};
use serde::{Deserialize, Serialize};

/// The gpx-owned slice of a `gpx` source. The folder to scan is
/// `fswalk.path`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GpxConfig {
    #[serde(default)]
    pub common: SourceCommon,

    #[serde(default)]
    pub fswalk: Option<LocalPath>,

    /// Gitignore-shaped patterns pruned from the scan, on top of any
    /// `.gitignore` files in the tree.
    #[serde(default)]
    pub ignore: Vec<String>,
}

impl GpxConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        Ok(())
    }
}

/// `gpx` renders nothing yet: its `plan_render` returns no processors.
pub type GpxRenderConfig = datalib_source_common::BareRenderConfig;

impl datalib_source_common::IngestMethods for GpxConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::local("fswalk")];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_keys_are_rejected() {
        let e = toml::from_str::<GpxConfig>("ignores = []").unwrap_err();
        assert!(e.to_string().contains("ignores"), "{e}");
    }

    #[test]
    fn ignore_round_trips() {
        let c: GpxConfig = toml::from_str("ignore = [\"old/\"]").unwrap();
        assert_eq!(c.ignore, ["old/"]);
    }
}
