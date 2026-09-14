// A metric series over time: appended when a value changed, at most
// every few seconds per series, plus the final value — so a rate is a
// query. Compacted like `disk_usage`: carry the last value forward.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, PortableTable)]
#[portable_table(
    table = "metric_samples",
    primary_key = "run_id, step, name, labels, ts_utc"
)]
pub struct MetricSampleRow {
    #[col(sql = "VARCHAR(64)")]
    pub run_id: String,
    #[col(sql = "VARCHAR(255)")]
    pub step: String,
    #[col(sql = "VARCHAR(64)")]
    pub name: String,
    #[col(sql = "VARCHAR(255)")]
    pub labels: String,
    /// UTC.
    #[col(sql = "VARCHAR(40)")]
    pub ts_utc: String,
    #[col(sql = "VARCHAR(8)")]
    pub tz_offset: Option<String>,
    #[col(sql = "BIGINT")]
    pub value: i64,
}
