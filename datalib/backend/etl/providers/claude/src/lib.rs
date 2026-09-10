//! Claude provider for [`datalib_etl`]: the download half — raw API
//! capture from claude.ai/api, plus the bulk-export ingest that writes
//! the same store. Rendering lives in [`datalib_etl_claude_render`].

pub mod ingest;
pub mod processor;
pub mod synthesize;
