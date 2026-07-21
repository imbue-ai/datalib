//! Cross-source download give-up bounds.
//!
//! The type itself now lives in the schema-only `frankweiler_source_common`
//! crate (so the orchestrator config and every provider `*-config` crate can
//! name it without pulling ETL code). It is re-exported here unchanged so the
//! shared HTTP retry chokepoint ([`crate::retry`]) and existing
//! `frankweiler_etl::download_params::DownloadParams` call sites keep resolving.

pub use frankweiler_source_common::download_params::DownloadParams;
