//! Notion provider for [`datalib_etl`]: the download half — raw API
//! capture from `api.notion.com`. Rendering lives in
//! [`datalib_etl_notion_render`].

pub mod ingest;
pub mod processor;
pub mod synthesize;
