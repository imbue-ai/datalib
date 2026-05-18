//! Frankweiler ETL crate.
//!
//! The pipeline is split into three stages, each driven by its own
//! binary:
//!
//! 1. **Extract** — `<provider>-download` captures raw API responses
//!    verbatim under `<out>/raw_api/`. Per-provider code lives in
//!    [`providers`].
//! 2. **Translate** — `<provider>-translate` reads `<out>/raw_api/` and
//!    emits per-document `.md` + `.grid_rows.json` sidecars under
//!    `<out>/rendered_md/<provider>/...`. Per-provider code lives in
//!    [`providers`].
//! 3. **Load** — `grid-rows-load` walks the sidecar tree and upserts
//!    rows into Dolt. Provider-agnostic; lives in [`load`].
//!
//! The cross-provider Translate→Load contract is [`sidecar::Sidecar`].
//! Incrementality is driven end-to-end by a `source_fingerprint` stamped
//! into each sidecar; the loader stores it in `documents_loaded` and
//! skips unchanged inputs on subsequent runs. Observability is wired
//! through [`obs::ObsArgs`] so every stage emits a comparable event
//! stream + OTLP traces.

pub mod load;
pub mod obs;
pub mod providers;
pub mod raw_store;
pub mod sidecar;
