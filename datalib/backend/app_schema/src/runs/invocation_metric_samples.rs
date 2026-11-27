// A metric series over time, per invocation, sampled as `metric_samples`
// was per run: when a value moved, at most every few seconds per series,
// plus the value it ended at.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, PortableTable)]
#[portable_table(
    table = "invocation_metric_samples",
    primary_key = "invocation_id, step, name, labels, ts_utc"
)]
pub struct InvocationMetricSampleRow {
    #[col(sql = "VARCHAR(64)")]
    pub invocation_id: String,
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
