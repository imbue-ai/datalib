// One row per reading, keyed `(ts_key, t)` so one time series' points
// sit together in the prolly tree and a sync's new points land in the
// leaves at its right edge.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, PortableTable)]
#[portable_table(table = "time_series_points", primary_key = "ts_key, t")]
pub struct TimeSeriesPointRow {
    #[col(sql = "INTEGER")]
    pub ts_key: i64,
    #[col(sql = "INTEGER")]
    pub t: i64,
    #[col(sql = "REAL")]
    pub value: Option<f64>,
    #[col(sql = "TEXT")]
    pub text: Option<String>,
}
