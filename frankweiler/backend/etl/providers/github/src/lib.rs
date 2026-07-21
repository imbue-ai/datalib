//! GitHub provider for [`frankweiler_etl`]: Download (raw API capture
//! from `api.github.com`) and Render (event-store JSONL → one
//! markdown document per PR + grid_rows sidecars). The Load step is
//! provider-agnostic and lives at [`frankweiler_etl::load`].

pub mod download;
pub mod processor;
pub mod render;
pub mod synthesize;
