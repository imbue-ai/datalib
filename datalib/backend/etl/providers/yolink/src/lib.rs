//! Yolink provider: the download half — per-device time-series CSVs
//! from `us.yosmart.com/download/...` into a doltlite raw store, a
//! window's readings and its coverage in one transaction. Rendering
//! lives in [`datalib_etl_yolink_render`].

pub mod ingest;
pub mod processor;
