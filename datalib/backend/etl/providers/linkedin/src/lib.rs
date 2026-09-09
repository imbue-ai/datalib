//! LinkedIn data-export ("takeout") provider: the download half.
//! Rendering lives in [`datalib_etl_linkedin_render`].

pub mod download;
pub mod processor;
pub mod synthesize;
