// One row per time series, in a source's store and, copied, in the
// index. `ts_key` is local to the source's store and keys its chunks
// there; `(source_id, ts_key)` names a time series anywhere.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

use crate::{TimeKind, ValueType};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, PortableTable)]
#[portable_table(
    table = "time_series",
    primary_key = "source_id, ts_key",
    index = "time_series_by_id:time_series_id"
)]
pub struct TimeSeriesRow {
    #[col(sql = "VARCHAR(128)")]
    pub source_id: String,
    #[col(sql = "INTEGER")]
    pub ts_key: i64,
    #[col(sql = "VARCHAR(96)")]
    pub time_series_id: String,
    #[col(sql = "VARCHAR(96)")]
    pub about_uuid: Option<String>,
    #[col(sql = "VARCHAR(128)")]
    pub name: String,
    #[col(sql = "TEXT")]
    pub human_name: String,
    #[col(sql = "VARCHAR(64)")]
    pub unit: String,
    #[col(sql = "VARCHAR(16)", enum)]
    pub value_type: ValueType,
    #[col(sql = "VARCHAR(16)", enum)]
    pub time_kind: TimeKind,
    #[col(sql = "INTEGER")]
    pub point_count: i64,
    #[col(sql = "INTEGER")]
    pub first_t: Option<i64>,
    #[col(sql = "INTEGER")]
    pub last_t: Option<i64>,
}
