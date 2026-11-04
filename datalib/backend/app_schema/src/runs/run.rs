// One row per run of the pipeline.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

/// What a run is for. A reader that meets a word this build does not know
/// keeps the word.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum RunKind {
    /// The loop serving what it was asked to sync.
    Sync,
    /// The first launch of a build asking each step to bring what it
    /// stored up to date. Nothing is downloaded.
    Migrate,
    /// Giving back disk: a compaction or a collection, from the command
    /// line.
    Shrink,
}

impl RunKind {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub fn parse(s: &str) -> Option<RunKind> {
        s.parse().ok()
    }
}

/// Why a run started, as whoever started it knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunStart {
    pub kind: RunKind,
    /// A sentence for a person: what asked, and for which steps.
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, PortableTable)]
#[portable_table(table = "runs", primary_key = "run_id")]
pub struct RunRow {
    /// A UUID the runner mints, or the job id when the app started the
    /// run — the same string every other table here keys on.
    #[col(sql = "VARCHAR(64)")]
    pub run_id: String,
    /// UTC.
    #[col(sql = "VARCHAR(40)")]
    pub started_at_utc: String,
    /// UTC; `None` while the run is going, or if it died.
    #[col(sql = "VARCHAR(40)")]
    pub finished_at_utc: Option<String>,
    /// The runner's own offset when it stamped the two above (`+02:00`).
    #[col(sql = "VARCHAR(8)")]
    pub tz_offset: Option<String>,
    /// A [`RunKind`] word; `None` for a run an older build recorded.
    #[col(sql = "VARCHAR(16)")]
    pub kind: Option<String>,
    /// [`RunStart::reason`].
    #[col(sql = "TEXT")]
    pub reason: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use strum::VariantArray;

    #[test]
    fn strum_and_serde_agree_on_a_runs_kind() {
        for kind in RunKind::VARIANTS {
            let word = serde_json::to_value(kind).unwrap();
            assert_eq!(word, kind.as_str());
            assert_eq!(RunKind::parse(kind.as_str()), Some(*kind));
        }
        assert_eq!(RunKind::parse("a kind from a newer build"), None);
    }
}
