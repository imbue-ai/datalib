//! Provider-owned config schema for the `markdown` source. Schema-only
//! (serde + anyhow), so the orchestrator can name [`MarkdownConfig`]
//! without linking the provider.

use datalib_source_common::{byte_size, LocalPath, SourceCommon};
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

/// Folders the scan never opens, on top of `ignore`: every folder whose
/// name starts with a dot. In a notes folder those hold an app's own
/// state (`.obsidian/`), its trash (`.trash/`) or a sync tool's old
/// copies of the notes (`.stversions/`), and a note found there would be
/// a second, stale copy of one found outside it.
pub const HIDDEN_FOLDERS: &str = ".*/";

/// The markdown-owned slice of a `markdown` source. The scan root is
/// `fswalk.path`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct MarkdownConfig {
    /// Shared per-source envelope (paths + cross-source tunables),
    /// resolved by the orchestrator's `normalize()`.
    #[serde(default)]
    pub common: SourceCommon,

    /// The folder to scan: every `.md` and `.markdown` file in it and in
    /// the folders inside it. Folders whose names start with a dot
    /// (`.obsidian`, `.trash`, `.git`) are never opened.
    #[serde(default)]
    pub fswalk: Option<LocalPath>,

    /// Gitignore-shaped patterns pruned from the scan, in addition to
    /// the `.gitignore` files inside a git repository and any `.ignore`
    /// files in the tree. Matched the way ripgrep
    /// matches them, so `**`, anchored `/`, and character classes all
    /// behave as expected.
    #[serde(default)]
    pub ignore: Vec<String>,

    /// Skip files larger than this — bytes, or a string like `"4 MB"`;
    /// 16 MiB unless set. A markdown file that large is nearly always an
    /// export or a log rather than a note.
    #[serde(
        default = "default_max_bytes",
        deserialize_with = "byte_size::deserialize_opt"
    )]
    #[schemars(with = "Option<byte_size::ByteSize>")]
    pub max_bytes: Option<u64>,
}

fn default_max_bytes() -> Option<u64> {
    Some(16 * 1024 * 1024)
}

impl MarkdownConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if let Some(0) = self.max_bytes {
            anyhow::bail!(
                "markdown: `max_bytes = 0` would skip every file; omit it for the 16 MiB \
                 default, or set a larger size"
            );
        }
        Ok(())
    }

    /// What the scan prunes: the hidden folders, then the config's own
    /// patterns.
    pub fn scan_ignores(&self) -> Vec<String> {
        std::iter::once(HIDDEN_FOLDERS.to_string())
            .chain(self.ignore.iter().cloned())
            .collect()
    }
}

/// Params for the render step. Render re-reads the raw store and needs
/// no provider-specific knobs of its own.
pub type MarkdownRenderConfig = datalib_source_common::BareRenderConfig;

impl datalib_source_common::IngestMethods for MarkdownConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::local("fswalk")];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_validates() {
        MarkdownConfig::default().validate().unwrap();
    }

    #[test]
    fn zero_max_bytes_is_rejected() {
        let c = MarkdownConfig {
            max_bytes: Some(0),
            ..Default::default()
        };
        let err = c.validate().unwrap_err().to_string();
        assert!(err.contains("16 MiB"), "{err}");
    }

    #[test]
    fn unknown_keys_are_rejected() {
        let e = toml::from_str::<MarkdownConfig>("ignroe = []").unwrap_err();
        assert!(e.to_string().contains("ignroe"), "{e}");
    }

    #[test]
    fn max_bytes_defaults_to_16mib_and_reads_unit_strings() {
        let c: MarkdownConfig = toml::from_str("").unwrap();
        assert_eq!(c.max_bytes, Some(16 * 1024 * 1024));
        let c: MarkdownConfig = toml::from_str(r#"max_bytes = "4 MiB""#).unwrap();
        assert_eq!(c.max_bytes, Some(4 * 1024 * 1024));
    }

    #[test]
    fn hidden_folders_are_pruned_before_the_configured_patterns() {
        let c: MarkdownConfig = toml::from_str(r#"ignore = ["templates/**"]"#).unwrap();
        assert_eq!(c.scan_ignores(), [".*/", "templates/**"]);
    }
}
