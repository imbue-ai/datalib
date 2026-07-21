//! Frankweiler ETL framework crate. Per-provider download +
//! render code lives in sibling crates named `frankweiler-etl-<provider>`
//! (e.g. [`frankweiler_etl_slack`]). The framework provides:
//!
//! - [`grid_index`] — the provider-agnostic grid-index (Load) step; ships as the
//!   `grid-rows-load` binary. The cross-provider render→grid-index
//!   wire contract (sidecar shape, `emit_sidecar` helper) now lives
//!   in the standalone `frankweiler-index-lib` crate; grid_index just
//!   reads through it.
//! - [`events`] — stable structured event vocabulary used by every
//!   download/render step. Initialization of the tracing subscriber
//!   that consumes these events lives in the shared `frankweiler_obs`
//!   crate so non-ETL binaries can use it too.
//!
//! Incrementality is driven end-to-end by a `source_fingerprint`
//! stamped into each sidecar; the loader stores it on
//! `documents.source_fingerprint` and skips unchanged inputs on
//! subsequent runs.

pub mod blob_cas;
pub mod bulk;
pub mod control;
pub mod doltlite_raw;
pub mod download_metrics;
pub mod download_params;
pub mod download_run;
pub mod event_store;
pub mod event_tape;
pub mod events;
pub mod file_checkpoint;
pub mod grid_index;
pub mod http;
pub mod ids;
pub mod indicatif_progress;
pub mod latchkey;
pub mod layout;
pub mod periodize;
pub mod processor;
pub mod progress;
pub mod raw_layout;
pub mod raw_store;
pub mod render_cursor;
pub mod retry;
pub mod scope_state;
pub mod section;
pub mod synthesize;
pub mod title;
