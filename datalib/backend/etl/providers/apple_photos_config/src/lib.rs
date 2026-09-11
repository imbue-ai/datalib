//! Provider-owned config schema for the `apple_photos` source.
//! Schema-only (serde + anyhow), so the orchestrator can name
//! [`ApplePhotosConfig`] without linking the provider.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use datalib_source_common::{glob_match, LocalPath, SourceCommon};
use serde::{Deserialize, Serialize};

/// Tables folded out by [`ApplePhotosConfig::skip_history`]. Core Data's
/// persistent history (`ACHANGE`, `ATRANSACTION`, `ATRANSACTIONSTRING`)
/// is a change log that grows on every write and that the mirror's own
/// `dolt_log` supersedes; the rest is the Photos daemons' work queue and
/// Core Data's per-entity counters and model cache. None of it says
/// anything about a photo, and together they are most of what moves
/// between two runs.
pub const HISTORY_TABLE_PATTERNS: &[&str] = &[
    "ACHANGE",
    "ATRANSACTION",
    "ATRANSACTIONSTRING",
    "ZBACKGROUNDJOBWORKITEM",
    "Z_PRIMARYKEY",
    "Z_METADATA",
    "Z_MODELCACHE",
];

/// Columns folded out by [`ApplePhotosConfig::skip_history`]: `Z_OPT` is
/// Core Data's optimistic-locking version counter, bumped on every save
/// of the row. It moves whenever anything else in the row does, so
/// dropping it loses nothing a diff would show.
pub const HISTORY_COLUMN_PATTERNS: &[&str] = &["*.Z_OPT"];

/// Inside a `.photoslibrary` bundle, where the database is.
pub const DATABASE_IN_BUNDLE: &str = "database/Photos.sqlite";

/// The apple_photos-owned slice of an `apple_photos` source. The library
/// is `library.path`; the doltlite mirror lands in the ingest step's
/// tree.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ApplePhotosConfig {
    /// Shared per-source envelope (paths + cross-source tunables),
    /// resolved by the orchestrator's `normalize()`.
    pub common: SourceCommon,

    /// The `.photoslibrary` bundle to mirror (or its `Photos.sqlite`
    /// directly; see [`photos_sqlite_path`]).
    pub library: Option<LocalPath>,

    /// Table-name globs to mirror. Default `["*"]` — every table in the
    /// library. Matched against the bare table name; `*` and `?` are the
    /// only metacharacters (see [`glob_match`]).
    pub include_tables: Vec<String>,

    /// Table-name globs to skip, applied after [`Self::include_tables`].
    pub exclude_tables: Vec<String>,

    /// `Table.column` globs to drop from the mirror. The column is absent
    /// from the mirrored table entirely — not blanked — so it costs
    /// nothing in the store and never shows up in a diff.
    pub exclude_columns: Vec<String>,

    /// Fold [`HISTORY_TABLE_PATTERNS`] and [`HISTORY_COLUMN_PATTERNS`]
    /// into the exclusions. On by default, unlike lightroom's
    /// `skip_xmp`: with it off, a library nobody touched still produces
    /// a commit on every run, because the daemons never stop writing
    /// their bookkeeping.
    pub skip_history: bool,

    /// Column names that, when present on a source table, are preferred
    /// over that table's declared primary key as the mirror's primary
    /// key. Photos never declares `ZUUID` UNIQUE, so the engine checks
    /// each run that it is, and keeps the declared key (with a warning)
    /// for any table where it is not.
    pub stable_key_columns: Vec<String>,

    /// Per-table primary-key override, `table -> [columns]`. Beats both
    /// [`Self::stable_key_columns`] and the declared key. An empty column
    /// list forces the table to be mirrored keyless.
    pub primary_keys: BTreeMap<String, Vec<String>>,

    /// Take a consistent snapshot (`VACUUM INTO`) of the library before
    /// reading it, instead of reading the live file. Photos' daemons
    /// hold the file open at all times, so this is the normal path.
    pub snapshot: bool,

    /// Collect unreachable chunks (`dolt_gc()`) at the start of each run.
    pub gc: bool,
}

impl Default for ApplePhotosConfig {
    fn default() -> Self {
        Self {
            common: SourceCommon::default(),
            library: None,
            include_tables: vec!["*".to_string()],
            exclude_tables: Vec::new(),
            exclude_columns: Vec::new(),
            skip_history: true,
            stable_key_columns: vec!["ZUUID".to_string()],
            primary_keys: BTreeMap::new(),
            snapshot: true,
            gc: false,
        }
    }
}

impl ApplePhotosConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.include_tables.is_empty() {
            anyhow::bail!("include_tables is empty: nothing would be mirrored");
        }
        Ok(())
    }

    pub fn effective_excluded_tables(&self) -> Vec<String> {
        let mut out = self.exclude_tables.clone();
        if self.skip_history {
            out.extend(HISTORY_TABLE_PATTERNS.iter().map(|s| s.to_string()));
        }
        out
    }

    pub fn effective_excluded_columns(&self) -> Vec<String> {
        let mut out = self.exclude_columns.clone();
        if self.skip_history {
            out.extend(HISTORY_COLUMN_PATTERNS.iter().map(|s| s.to_string()));
        }
        out
    }

    pub fn wants_table(&self, table: &str) -> bool {
        let excluded = self.effective_excluded_tables();
        self.include_tables.iter().any(|p| glob_match(p, table))
            && !excluded.iter().any(|p| glob_match(p, table))
    }
}

/// The SQLite file for a configured library path: the bundle's
/// `database/Photos.sqlite` when handed the `.photoslibrary` (what the
/// file picker returns), the path itself when handed the database
/// directly. Pure — nothing is stat'ed — so a config can be checked
/// without the library present.
pub fn photos_sqlite_path(library: &Path) -> PathBuf {
    let is_bundle = library
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("photoslibrary"));
    if is_bundle {
        library.join(DATABASE_IN_BUNDLE)
    } else {
        library.to_path_buf()
    }
}

/// Params for the render step. `apple_photos` is download-only (see the
/// provider crate's `processor`), so this is the shared bare envelope.
pub type ApplePhotosRenderConfig = datalib_source_common::BareRenderConfig;

impl datalib_source_common::IngestMethods for ApplePhotosConfig {
    const METHODS: &'static [datalib_source_common::IngestMethod] =
        &[datalib_source_common::IngestMethod::local("library")];
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_skip_history_and_key_on_zuuid() {
        let c = ApplePhotosConfig::default();
        assert!(c.wants_table("ZASSET"));
        assert!(c.wants_table("ZGENERICALBUM"));
        assert!(!c.wants_table("ACHANGE"));
        assert!(!c.wants_table("ZBACKGROUNDJOBWORKITEM"));
        assert_eq!(c.effective_excluded_columns(), vec!["*.Z_OPT".to_string()]);
        assert_eq!(c.stable_key_columns, vec!["ZUUID".to_string()]);
    }

    #[test]
    fn skip_history_off_is_a_faithful_mirror() {
        let c = ApplePhotosConfig {
            skip_history: false,
            ..Default::default()
        };
        assert!(c.wants_table("ACHANGE"));
        assert!(c.effective_excluded_columns().is_empty());
    }

    #[test]
    fn bundle_or_database_path_both_resolve_to_the_database() {
        let bundle = Path::new("/Users/x/Pictures/Photos Library.photoslibrary");
        assert_eq!(
            photos_sqlite_path(bundle),
            bundle.join("database/Photos.sqlite")
        );
        let db = Path::new("/tmp/copy/Photos.sqlite");
        assert_eq!(photos_sqlite_path(db), db);
    }

    #[test]
    fn empty_include_list_is_rejected() {
        let c = ApplePhotosConfig {
            include_tables: Vec::new(),
            ..Default::default()
        };
        assert!(c.validate().is_err());
    }
}
