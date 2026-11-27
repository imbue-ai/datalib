// One row per busy period of the loop, as builds before invocations
// recorded them. This build writes none and reads them as they are.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

/// What an older build's run was for. A reader that meets a word this
/// build does not know keeps the word.
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
    /// The prepare pass when the loop starts: each step whose program
    /// takes `--prepare` has its command run with `--prepare` appended,
    /// to bring what it stored to this build's shape. Nothing is
    /// downloaded.
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

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, PortableTable)]
#[portable_table(table = "runs", primary_key = "run_id")]
pub struct RunRow {
    /// A UUID the runner minted, the key of the run's rows in
    /// `step_runs`, `metrics`, `metric_samples`, `log` and `processes`.
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
    /// A sentence for a person: who or what started the run.
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
