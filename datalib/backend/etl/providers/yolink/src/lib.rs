//! Yolink provider: the download half — per-device time-series CSVs
//! from `us.yosmart.com/download/...` into a doltlite raw store, one
//! `dolt_commit` per window so re-fetches that change historical values
//! land as auditable diffs in `dolt log`. Rendering lives in
//! [`datalib_etl_yolink_render`].

pub mod ingest;
pub mod processor;
