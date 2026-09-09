//! GitLab provider for [`datalib_etl`]: the download half — raw API
//! capture from a GitLab instance. Rendering lives in
//! [`datalib_etl_gitlab_render`].

pub mod download;
pub mod processor;
pub mod synthesize;
