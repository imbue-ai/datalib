// The current value of every metric series a step reported in one
// invocation. A value the runner keeps for a step between its invocations
// (a consumer's `queued{from=…}`) is filed under the step's latest one.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, PortableTable)]
#[portable_table(
    table = "invocation_metrics",
    primary_key = "invocation_id, step, name, labels"
)]
pub struct InvocationMetricRow {
    #[col(sql = "VARCHAR(64)")]
    pub invocation_id: String,
    #[col(sql = "VARCHAR(255)")]
    pub step: String,
    #[col(sql = "VARCHAR(64)")]
    pub name: String,
    /// As `metrics.labels`: `k=v` pairs joined with `,` in key order.
    #[col(sql = "VARCHAR(255)")]
    pub labels: String,
    #[col(sql = "BIGINT")]
    pub value: i64,
    /// UTC.
    #[col(sql = "VARCHAR(40)")]
    pub updated_at_utc: String,
    #[col(sql = "VARCHAR(8)")]
    pub tz_offset: Option<String>,
}
