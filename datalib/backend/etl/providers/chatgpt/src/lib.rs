//! ChatGPT provider for [`datalib_etl`]: Download (raw API
//! capture from chatgpt.com/backend-api) and Render (raw →
//! per-conversation markdown + rows in the render store). The Load step
//! is provider-agnostic and lives at [`datalib_etl::load`].

pub mod download;
pub mod processor;
pub mod render;
pub mod synthesize;
