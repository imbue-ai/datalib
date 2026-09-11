//! ChatGPT provider for [`datalib_etl`]: the download half — raw API
//! capture from chatgpt.com/backend-api. Rendering lives in
//! [`datalib_etl_chatgpt_render`].

pub mod ingest;
pub mod probe;
pub mod processor;
pub mod synthesize;
