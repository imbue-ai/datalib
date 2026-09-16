//! The render half of the `claude_code` provider: raw store → markdown
//! and `grid_rows`, one document per transcript. The ingest half is
//! [`datalib_etl_claude_code`].

pub mod ids;
pub mod processor;
pub mod render;
