//! The render half of the `github` provider: raw store -> markdown
//! and `grid_rows`. The download half is [`datalib_etl_github`].

pub mod processor;
pub mod render;
