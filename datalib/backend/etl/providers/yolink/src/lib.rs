//! Yolink provider: pulls per-device time-series CSVs from
//! `us.yosmart.com/download/...` into a doltlite raw store, one
//! `dolt_commit` per window so re-fetches that change historical
//! values land as auditable diffs in `dolt log`.
//!
//! The render side collapses that whole store into a **single**
//! markdown page: a summary of everything non-timeseries the store
//! knows, plus one interactive Plotly scatter per physical quantity
//! (temperature, humidity, liquid volume) with every device as a
//! series, converted to SI. Because there is exactly one page, the
//! render cursor is just the store's HEAD commit — see
//! [`render::parse`]. Start at `download/mod.rs` for the fetch and
//! `render/mod.rs` for the page (both are short).

pub mod download;
pub mod processor;
pub mod render;
