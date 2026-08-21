//! Provider-owned config schema for the `lightroom` source.
//! Schema-only (serde + anyhow), so the orchestrator can name
//! [`LightroomConfig`] without linking the provider.
//!
//! `lightroom` is purely file-backed: the input is an Adobe Lightroom
//! Classic catalog (`*.lrcat`), which is an ordinary SQLite database.
//! There is no API and no `sync:` block — `common.input_path` points at
//! the catalog and everything else here is a filter or a key-selection
//! knob.
//!
//! The engine behind it is deliberately generic ("mirror every table of
//! a SQLite file into a doltlite store"); nothing in this schema is
//! Lightroom-specific except the [`XMP_COLUMN_PATTERNS`] preset and the
//! default of `id_global` in [`LightroomConfig::stable_key_columns`].
//! See the provider crate's `INGEST.md`.

use std::collections::BTreeMap;

use datalib_source_common::SourceCommon;
use serde::{Deserialize, Serialize};

/// Columns folded in by [`LightroomConfig::skip_xmp`]. These are the
/// bulky, wholly-derived metadata blobs in a Lightroom catalog: the
/// serialized XMP packet Lightroom keeps per image, and the flattened
/// search-index strings it rebuilds from the harvested EXIF/IPTC tables.
///
/// Everything here is reconstructible from the columns that remain, so
/// dropping it costs fidelity of the *catalog file* but not of the
/// *catalog's information*. On a real catalog `Adobe_AdditionalMetadata.xmp`
/// alone is routinely the single largest column in the file.
pub const XMP_COLUMN_PATTERNS: &[&str] = &[
    "Adobe_AdditionalMetadata.xmp",
    "AgMetadataSearchIndex.*SearchIndex",
    "AgMetadataSearchIndex.searchIndex",
];

/// The lightroom-owned slice of a `lightroom` source. The catalog is
/// `common.input_path`; the doltlite mirror lands in `common.raw_path`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct LightroomConfig {
    /// Shared per-source envelope (paths + cross-source tunables),
    /// resolved by the orchestrator's `normalize()`. The catalog to
    /// mirror is `input_path`.
    pub common: SourceCommon,

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
    ///
    /// Off by default: a backup should be a faithful mirror unless the
    /// user says otherwise. Turn it on when catalog size matters more
    /// than being able to reconstruct the `.lrcat` byte-for-byte.
    pub skip_xmp: bool,

    /// Column names that, when present as a single-column UNIQUE index on
    /// a source table, are preferred over that table's declared primary
    /// key as the mirror's primary key. First match in this list wins.
    ///
    /// This is the answer to "what if the primary key changes". Lightroom
    /// tables are keyed by `id_local INTEGER PRIMARY KEY` — a rowid alias
    /// that Lightroom is free to renumber on a catalog upgrade or
    /// optimize — alongside a stable `id_global UNIQUE NOT NULL` UUID.
    /// Keying the mirror on `id_local` would turn a renumbering into
    /// "every row deleted and re-added"; keying it on `id_global` turns
    /// the same event into a one-column modification per row, which is
    /// both a truthful diff and a cheap one to store.
    ///
    /// Set to `[]` to mirror each table's declared primary key verbatim.
    pub stable_key_columns: Vec<String>,

    /// Per-table primary-key override, `table -> [columns]`. Beats both
    /// [`Self::stable_key_columns`] and the declared key. An empty column
    /// list forces the table to be mirrored keyless.
    pub primary_keys: BTreeMap<String, Vec<String>>,

    /// Take a consistent snapshot (`VACUUM INTO`) of the catalog before
    /// reading it, instead of reading the live file.
    ///
    /// On by default. Lightroom holds its catalog open — and in WAL mode
    /// — while running, so reading the live file can otherwise observe a
    /// torn view or fail on a lock. The snapshot is written to a temp
    /// directory and deleted when the run ends.
    pub snapshot: bool,

    /// Collect unreachable chunks (`dolt_gc()`) at the start of each run.
    ///
    /// Off by default: it rewrites the whole chunk store, which is time a
    /// routine no-op run shouldn't spend. Turn it on — or run it by hand
    /// with the doltlite shell — when store size matters. It is not a
    /// history trade-off: `dolt_log` and `dolt_history_*` survive intact,
    /// and on a 3.3 MB catalog with two versions of history it took the
    /// store from 5.2 MB to 1.3 MB.
    pub gc: bool,
}

impl Default for LightroomConfig {
    fn default() -> Self {
        Self {
            common: SourceCommon::default(),
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

    /// The effective column-exclusion patterns: the configured list plus
    /// the XMP preset when [`Self::skip_xmp`] is set.
    pub fn effective_excluded_columns(&self) -> Vec<String> {
        let mut out = self.exclude_columns.clone();
        if self.skip_xmp {
            out.extend(XMP_COLUMN_PATTERNS.iter().map(|s| s.to_string()));
        }
        out
    }

    /// Should this table be mirrored?
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

/// Minimal glob match: `*` (any run, including empty) and `?` (exactly
/// one character). Everything else is literal, and matching is
/// case-sensitive — SQLite identifiers here come straight out of
/// `sqlite_master`, so the user sees exactly what they must type.
///
/// Deliberately hand-rolled rather than pulling in `globset`: two
/// metacharacters over identifier-shaped strings is the whole
/// requirement, and the crate universe doesn't expose a glob crate today.
pub fn glob_match(pattern: &str, text: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    // Classic two-pointer backtracking matcher: O(len(p) * len(t)) worst
    // case, O(n) on patterns without adjacent stars, and no allocation.
    let (mut pi, mut ti) = (0usize, 0usize);
    // Where to resume if the current `*` guess turns out to be too short.
    let (mut star, mut star_ti) = (None::<usize>, 0usize);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            star_ti = ti;
            pi += 1;
        } else if let Some(s) = star {
            // Backtrack: let the star swallow one more character.
            pi = s + 1;
            star_ti += 1;
            ti = star_ti;
        } else {
            return false;
        }
    }
    // Trailing stars in the pattern match the empty remainder.
    p[pi..].iter().all(|c| *c == '*')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_literals_and_wildcards() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("*", ""));
        assert!(glob_match("Adobe_images", "Adobe_images"));
        assert!(!glob_match("Adobe_images", "Adobe_imageProperties"));
        assert!(glob_match("Ag*", "AgLibraryFile"));
        assert!(!glob_match("Ag*", "Adobe_images"));
        assert!(glob_match("*.xmp", "Adobe_AdditionalMetadata.xmp"));
        assert!(glob_match(
            "AgMetadataSearchIndex.*SearchIndex",
            "AgMetadataSearchIndex.exifSearchIndex"
        ));
        assert!(!glob_match(
            "AgMetadataSearchIndex.*SearchIndex",
            "AgMetadataSearchIndex.image"
        ));
        assert!(glob_match("?g*", "AgLibraryFile"));
        assert!(!glob_match("?g*", "Adobe_images"));
        // The backtracking case: a star that must give up its first guess.
        assert!(glob_match("*Oz*Ids", "AgLibraryImageOzAssetIds"));
        assert!(!glob_match("*Oz*Ids", "AgLibraryImageOzAsset"));
    }

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
