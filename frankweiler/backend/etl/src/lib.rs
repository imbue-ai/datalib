//! Frankweiler ETL framework crate. Per-provider Extract + Translate
//! code lives in sibling crates named `frankweiler-etl-<provider>`
//! (e.g. [`frankweiler_etl_slack`]). The framework provides:
//!
//! - [`load`] — the provider-agnostic Load step; ships as the
//!   `grid-rows-load` binary. The cross-provider Translate→Load
//!   wire contract (sidecar shape, `emit_sidecar` helper) now lives
//!   in the standalone `frankweiler-index-lib` crate; load just
//!   reads through it.
//! - [`events`] — stable structured event vocabulary used by every
//!   Extract/Translate step. Initialization of the tracing subscriber
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
pub mod event_store;
pub mod event_tape;
pub mod events;
pub mod extract_run;
pub mod file_checkpoint;
pub mod http;
pub mod ids;
pub mod latchkey;
pub mod load;
pub mod periodize;
pub mod progress;
pub mod render_cursor;
pub mod scope_state;
pub mod section;
pub mod synthesize;
pub mod title;
