//! The render half of the `claude` provider: raw store -> markdown
//! and `grid_rows`. The download half is [`datalib_etl_claude`].

pub mod processor;
pub mod render;
