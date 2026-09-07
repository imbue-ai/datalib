//! Notion provider for [`datalib_etl`]: Download (raw API capture
//! from `api.notion.com`)
//! and Render (event-store JSONL → per-page markdown + grid_rows
//! sidecars). The Load step is provider-agnostic and lives at
//! [`datalib_etl::load`].

pub mod download;
pub mod processor;
pub mod render;
pub mod synthesize;
