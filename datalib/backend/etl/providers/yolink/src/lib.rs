//! Yolink provider: pulls per-device time-series CSVs from
//! `us.yosmart.com/download/...` into a doltlite raw store, one
//! `dolt_commit` per window so re-fetches that change historical
//! values land as auditable diffs in `dolt log`.

pub mod download;
pub mod processor;
pub mod render;
