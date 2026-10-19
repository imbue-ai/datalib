// A run of one time series' points, encoded into one blob, with what a
// reader needs to skip or summarise it undecoded. Only a source's store
// has this table; the index points at it.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, PortableTable)]
#[portable_table(table = "time_series_chunks", primary_key = "ts_key, first_t")]
pub struct TimeSeriesChunkRow {
    #[col(sql = "INTEGER")]
    pub ts_key: i64,
    #[col(sql = "INTEGER")]
    pub first_t: i64,
    #[col(sql = "INTEGER")]
    pub last_t: i64,
    #[col(sql = "INTEGER")]
    pub n: i64,
    /// Over the values as numbers; null for text.
    #[col(sql = "REAL")]
    pub min: Option<f64>,
    #[col(sql = "REAL")]
    pub max: Option<f64>,
    #[col(sql = "REAL")]
    pub sum: Option<f64>,
    /// Which layout `points` is in; a reader refuses one it does not know.
    #[col(sql = "INTEGER")]
    pub encoding: i64,
    #[col(sql = "BLOB")]
    pub points: Vec<u8>,
}
