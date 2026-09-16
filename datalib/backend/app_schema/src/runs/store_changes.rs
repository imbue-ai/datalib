// One row per part of the store, counting its writes — the store's own
// record of what moved, for a watcher that sees the file change and has
// to say which readers care.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

/// The parts of the store a reader can depend on separately. `log` is
/// two of them: a run's lines feed the Manage screen's rows (error
/// counts, how long since a step last spoke), the server's own lines
/// feed nothing but the log grid.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum StorePart {
    Runs,
    StepRuns,
    Metrics,
    RunLog,
    ProcessLog,
}

impl StorePart {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    pub fn parse(s: &str) -> Option<StorePart> {
        s.parse().ok()
    }
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, PortableTable)]
#[portable_table(table = "store_changes", primary_key = "what")]
pub struct StoreChangeRow {
    /// A [`StorePart`], as text.
    #[col(sql = "VARCHAR(32)")]
    pub what: String,
    /// Bumped by one per write. Only ever compared for inequality.
    #[col(sql = "BIGINT")]
    pub version: i64,
    /// UTC; when it last moved.
    #[col(sql = "VARCHAR(40)")]
    pub changed_at_utc: String,
    #[col(sql = "VARCHAR(8)")]
    pub tz_offset: Option<String>,
}
