// The index's record of each source store it read: where the store is
// and the commit it was read at, which is the commit a reader of the
// index opens it at.

use datalib_etl_macros::PortableTable;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, PortableTable)]
#[portable_table(table = "time_series_sources", primary_key = "source_id")]
pub struct TimeSeriesSourceRow {
    #[col(sql = "VARCHAR(128)")]
    pub source_id: String,
    /// The store's path under the data root, `/`-separated.
    #[col(sql = "TEXT")]
    pub store: String,
    #[col(sql = "VARCHAR(64)")]
    pub commit_hash: String,
}
