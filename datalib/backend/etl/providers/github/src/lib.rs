//! GitHub provider for [`datalib_etl`]: Download (raw API capture
//! from `api.github.com`) and Render (event-store JSONL → one
//! markdown document per PR + rows in the render store). The Load step is
//! provider-agnostic and lives at [`datalib_etl::load`].

pub mod download;
pub mod processor;
pub mod render;
pub mod synthesize;
