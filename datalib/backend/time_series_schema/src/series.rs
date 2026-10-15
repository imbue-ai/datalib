// One row per time series. `ts_key` is local to the store the row is in
// and is what its points are keyed by: an integer key keeps a point to
// about 48 bytes on disk, against about 115 under the uuid
// (`hack/doltlite_time_series_cost/`).

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

use crate::TimeKind;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, PortableTable)]
#[portable_table(
    table = "time_series",
    primary_key = "ts_key",
    index = "time_series_by_id:time_series_id"
)]
pub struct TimeSeriesRow {
    #[col(sql = "INTEGER")]
    pub ts_key: i64,
    #[col(sql = "VARCHAR(96)")]
    pub time_series_id: String,
    #[col(sql = "VARCHAR(128)")]
    pub source_id: String,
    #[col(sql = "VARCHAR(256)")]
    pub entity: String,
    #[col(sql = "VARCHAR(128)")]
    pub metric: String,
    #[col(sql = "TEXT")]
    pub label: String,
    #[col(sql = "VARCHAR(32)")]
    pub unit: String,
    #[col(sql = "VARCHAR(16)", enum)]
    pub time_kind: TimeKind,
    #[col(sql = "VARCHAR(64)")]
    pub points_hash: String,
    #[col(sql = "INTEGER")]
    pub point_count: i64,
    #[col(sql = "INTEGER")]
    pub first_t: Option<i64>,
    #[col(sql = "INTEGER")]
    pub last_t: Option<i64>,
}
