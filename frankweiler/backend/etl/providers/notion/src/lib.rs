//! Notion provider for [`frankweiler_etl`]: Download (raw API capture
//! from `api.notion.com` + `www.notion.so/api/v3` for inbox discovery)
//! and Render (event-store JSONL → per-page markdown + grid_rows
//! sidecars). The Load step is provider-agnostic and lives at
//! [`frankweiler_etl::load`].

pub mod download;
pub mod processor;
pub mod render;
pub mod synthesize;
