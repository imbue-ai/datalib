//! Frankweiler ETL framework crate. Per-provider Extract + Translate
//! code lives in sibling crates named `frankweiler-etl-<provider>`
//! (e.g. [`frankweiler_etl_slack`]). The framework provides:
//!
//! - [`sidecar`] — the cross-provider Translate→Load contract.
//! - [`load`] — the provider-agnostic Load step; ships as the
//!   `grid-rows-load` binary.
//! - [`obs`] — shared `ObsArgs` for clap flatten + tracing/OTLP
//!   initialization so every stage emits a comparable event stream.
//! - [`raw_store`] — content-addressed page capture used by every
//!   Extract step.
//!
//! Incrementality is driven end-to-end by a `source_fingerprint`
//! stamped into each sidecar; the loader stores it in
//! `documents_loaded` and skips unchanged inputs on subsequent runs.

pub mod event_store;
pub mod http;
pub mod load;
pub mod obs;
pub mod raw_store;
pub mod sidecar;
pub mod synthesize;
