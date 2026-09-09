//! The render half of the `notion` provider: raw store -> markdown
//! and `grid_rows`. The download half is [`datalib_etl_notion`].

pub mod processor;
pub mod render;
