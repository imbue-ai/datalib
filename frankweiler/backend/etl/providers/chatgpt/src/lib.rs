//! ChatGPT provider for [`frankweiler_etl`]: Extract (raw API
//! capture from chatgpt.com/backend-api) and Translate (raw →
//! per-conversation markdown + grid_rows sidecars). The Load step
//! is provider-agnostic and lives at [`frankweiler_etl::load`].

pub mod extract;
pub mod translate;
