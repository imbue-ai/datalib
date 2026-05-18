//! Anthropic provider for [`frankweiler_etl`]: Extract (raw API
//! capture from claude.ai/api) and Translate (raw → per-conversation
//! markdown + grid_rows sidecars). The Load step is provider-agnostic
//! and lives at [`frankweiler_etl::load`].

pub mod extract;
pub mod translate;
