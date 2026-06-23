//! GitHub provider for [`frankweiler_etl`]: Extract (raw API capture
//! from `api.github.com`) and Translate (event-store JSONL → one
//! markdown document per PR + grid_rows sidecars). The Load step is
//! provider-agnostic and lives at [`frankweiler_etl::load`].

pub mod extract;
pub mod render_and_index_md;
pub mod synthesize;
