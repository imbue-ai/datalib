//! ChatGPT provider for [`frankweiler_etl`]: Download (raw API
//! capture from chatgpt.com/backend-api) and Render (raw →
//! per-conversation markdown + grid_rows sidecars). The Load step
//! is provider-agnostic and lives at [`frankweiler_etl::load`].

pub mod download;
pub mod processor;
pub mod render;
pub mod synthesize;
