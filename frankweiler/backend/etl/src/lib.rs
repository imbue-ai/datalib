//! Frankweiler ETL framework crate. Per-provider Extract + Translate
//! code lives in sibling crates named `frankweiler-etl-<provider>`
//! (e.g. [`frankweiler_etl_slack`]). The framework provides:
//!
//! - [`sidecar`] — the cross-provider Translate→Load contract.
//! - [`load`] — the provider-agnostic Load step; ships as the
//!   `grid-rows-load` binary.
//! - [`events`] — stable structured event vocabulary used by every
//!   Extract/Translate step. Initialization of the tracing subscriber
//!   that consumes these events lives in the shared `frankweiler_obs`
//!   crate so non-ETL binaries can use it too.
//! - [`raw_store`] — content-addressed page capture used by every
//!   Extract step.
//!
//! Incrementality is driven end-to-end by a `source_fingerprint`
//! stamped into each sidecar; the loader stores it on
//! `documents.source_fingerprint` and skips unchanged inputs on
//! subsequent runs.

pub mod blob_store;
pub mod blobs;
pub mod doltlite_raw;
pub mod event_store;
pub mod events;
pub mod http;
pub mod ids;
pub mod latchkey;
pub mod load;
pub mod progress;
pub mod raw_store;
pub mod sidecar;
pub mod synthesize;
