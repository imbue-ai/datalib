//! The render half of the `codex` provider: raw store → markdown and
//! `grid_rows`, one document per thread. The ingest half is
//! [`datalib_etl_codex`].

pub mod ids;
pub mod processor;
pub mod render;
