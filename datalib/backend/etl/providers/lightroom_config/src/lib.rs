//! Provider-owned config schema for the `lightroom` source.
//! Schema-only (serde + anyhow), so the orchestrator can name
//! [`LightroomConfig`] without linking the provider.

use std::collections::BTreeMap;

use datalib_source_common::{LocalPath, SourceCommon};
use serde::{Deserialize, Serialize};

/// Columns folded in by [`LightroomConfig::skip_xmp`]. These are the
/// bulky, wholly-derived metadata blobs in a Lightroom catalog: the
/// serialized XMP packet Lightroom keeps per image, and the flattened
/// search-index strings it rebuilds from the harvested EXIF/IPTC tables.
pub const XMP_COLUMN_PATTERNS: &[&str] = &[
    "Adobe_AdditionalMetadata.xmp",
    "AgMetadataSearchIndex.*SearchIndex",
    "AgMetadataSearchIndex.searchIndex",
];

/// The lightroom-owned slice of a `lightroom` source. The catalog is
/// `catalog.path`; the doltlite mirror lands in the ingest step's tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LightroomConfig {
    /// Shared per-source envelope (paths + cross-source tunables),
    /// resolved by the orchestrator's `normalize()`.
    pub common: SourceCommon,

    /// The `.lrcat` to mirror.
    pub catalog: Option<LocalPath>,

    /// Table-name globs to mirror. Default `["*"]` — every table in the
    /// catalog. Matched against the bare table name; `*` and `?` are the
    /// only metacharacters (see [`glob_match`]).
    pub include_tables: Vec<String>,

    /// Table-name globs to skip, applied after [`Self::include_tables`].
    /// Default empty: mirror everything.
    pub exclude_tables: Vec<String>,

    /// `Table.column` globs to drop from the mirror. The column is absent
    /// from the mirrored table entirely — not blanked — so it costs
    /// nothing in the store and never shows up in a diff.
    pub exclude_columns: Vec<String>,

    /// Fold [`XMP_COLUMN_PATTERNS`] into [`Self::exclude_columns`].
    pub skip_xmp: bool,

    /// Column names that, when present as a single-column UNIQUE index on
    /// a source table, are preferred over that table's declared primary
    /// key as the mirror's primary key. First match in this list wins.
    pub stable_key_columns: Vec<String>,

    /// Per-table primary-key override, `table -> [columns]`. Beats both
    /// [`Self::stable_key_columns`] and the declared key. An empty column
    /// list forces the table to be mirrored keyless.
    pub primary_keys: BTreeMap<String, Vec<String>>,

    /// Take a consistent snapshot (`VACUUM INTO`) of the catalog before
    /// reading it, instead of reading the live file.
    pub snapshot: bool,

    /// Collect unreachable chunks (`dolt_gc()`) at the start of each run.
    pub gc: bool,
}

impl Default for LightroomConfig {
    fn default() -> Self {
        Self {
            common: SourceCommon::default(),
            catalog: None,
            include_tables: vec!["*".to_string()],
            exclude_tables: Vec::new(),
            exclude_columns: Vec::new(),
            skip_xmp: false,
            stable_key_columns: vec!["id_global".to_string()],
            primary_keys: BTreeMap::new(),
            snapshot: true,
            gc: false,
        }
    }
}

impl LightroomConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.include_tables.is_empty() {
            anyhow::bail!("include_tables is empty: nothing would be mirrored");
        }
        Ok(())
    }

    pub fn effective_excluded_columns(&self) -> Vec<String> {
        let mut out = self.exclude_columns.clone();
        if self.skip_xmp {
            out.extend(XMP_COLUMN_PATTERNS.iter().map(|s| s.to_string()));
        }
        out
    }

    pub fn wants_table(&self, table: &str) -> bool {
        self.include_tables.iter().any(|p| glob_match(p, table))
            && !self.exclude_tables.iter().any(|p| glob_match(p, table))
    }

    /// Should this column be mirrored? `patterns` comes from
    /// [`Self::effective_excluded_columns`] (hoisted by the caller so the
    /// preset isn't re-expanded per column).
    pub fn wants_column(&self, patterns: &[String], table: &str, column: &str) -> bool {
        let qualified = format!("{table}.{column}");
        !patterns.iter().any(|p| glob_match(p, &qualified))
    }
}

/// Params for the render step. `lightroom` is download-only for now (see
/// the provider crate's `processor::plan_render`), so this is the shared
/// bare envelope.
pub type LightroomRenderConfig = datalib_source_common::BareRenderConfig;

pub use datalib_source_common::glob_match;

impl datalib_source_common::IngestMethods for LightroomConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::local("catalog")];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_mirror_everything() {
        let c = LightroomConfig::default();
        assert!(c.wants_table("Adobe_images"));
        assert!(c.wants_table("AgLibraryFile"));
        let pats = c.effective_excluded_columns();
        assert!(pats.is_empty(), "skip_xmp is off by default");
        assert!(c.wants_column(&pats, "Adobe_AdditionalMetadata", "xmp"));
    }

    #[test]
    fn skip_xmp_drops_the_bulky_derived_columns_only() {
        let c = LightroomConfig {
            skip_xmp: true,
            ..Default::default()
        };
        let pats = c.effective_excluded_columns();
        assert!(!c.wants_column(&pats, "Adobe_AdditionalMetadata", "xmp"));
        assert!(!c.wants_column(&pats, "AgMetadataSearchIndex", "exifSearchIndex"));
        assert!(!c.wants_column(&pats, "AgMetadataSearchIndex", "searchIndex"));
        // Neighbouring columns in the same tables survive.
        assert!(c.wants_column(&pats, "Adobe_AdditionalMetadata", "internalXmpDigest"));
        assert!(c.wants_column(&pats, "AgMetadataSearchIndex", "image"));
    }

    #[test]
    fn exclude_beats_include() {
        let c = LightroomConfig {
            include_tables: vec!["Ag*".into()],
            exclude_tables: vec!["*Oz*".into()],
            ..Default::default()
        };
        assert!(c.wants_table("AgLibraryFile"));
        assert!(!c.wants_table("AgLibraryImageOzAssetIds"));
        assert!(!c.wants_table("Adobe_images"));
    }

    #[test]
    fn empty_include_list_is_rejected() {
        let c = LightroomConfig {
            include_tables: Vec::new(),
            ..Default::default()
        };
        assert!(c.validate().is_err());
    }
}
