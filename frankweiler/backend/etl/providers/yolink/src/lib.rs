//! Yolink provider: pulls per-device time-series CSVs from
//! `us.yosmart.com/download/...` into a doltlite raw store, one
//! `dolt_commit` per window so re-fetches that change historical
//! values land as auditable diffs in `dolt log`.
//!
//! No translate / render path — the readings table is queried
//! directly by downstream tools. See `extract/mod.rs` for the full
//! story (it's short).

pub mod extract;
pub mod processor;
