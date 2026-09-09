//! The render half of the `gitlab` provider: raw store -> markdown
//! and `grid_rows`. The download half is [`datalib_etl_gitlab`].

pub mod processor;
pub mod render;
