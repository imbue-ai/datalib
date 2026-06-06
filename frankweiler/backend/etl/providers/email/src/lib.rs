//! JMAP provider for [`frankweiler_etl`]: Extract (raw API capture into
//! a single doltlite db) and Translate (raw → per-thread markdown +
//! `grid_rows` sidecars). The Load step is provider-agnostic and lives
//! at [`frankweiler_etl::load`].
//!
//! Scope: generic JMAP-mail (RFC 8620 core + RFC 8621 mail). Today the
//! only server we test against is api.fastmail.com, but every
//! transport-level decision is keyed on values from the JMAP session
//! object (`apiUrl`, `downloadUrl`, `primaryAccounts`, …) — no
//! Fastmail-specific URLs are hardcoded.

pub mod extract;
pub mod synthesize;
pub mod translate;

pub use extract::db;
