// The current value of every metric series a step reported this run.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize, PortableTable)]
#[portable_table(table = "metrics", primary_key = "run_id, step, name, labels")]
pub struct MetricRow {
    #[col(sql = "VARCHAR(64)")]
    pub run_id: String,
    #[col(sql = "VARCHAR(255)")]
    pub step: String,
    #[col(sql = "VARCHAR(64)")]
    pub name: String,
    /// The labels canonicalized to one string (`table=slack_messages`,
    /// `k=v` pairs joined with `,` in key order), empty for a series
    /// with none — so the set can be part of the key.
    #[col(sql = "VARCHAR(255)")]
    pub labels: String,
    #[col(sql = "BIGINT")]
    pub value: i64,
    /// UTC.
    #[col(sql = "VARCHAR(40)")]
    pub updated_at: String,
    #[col(sql = "VARCHAR(8)")]
    pub tz_offset: Option<String>,
}
