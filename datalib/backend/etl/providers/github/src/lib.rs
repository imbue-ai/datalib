//! GitHub provider for [`datalib_etl`]: the download half — raw API
//! capture from `api.github.com`. Rendering lives in
//! [`datalib_etl_github_render`].

pub mod download;
pub mod processor;
pub mod synthesize;
