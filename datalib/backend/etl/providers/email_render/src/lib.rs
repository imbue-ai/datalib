//! The render half of the `email` provider: raw store -> markdown
//! and `grid_rows`. The download half is [`datalib_etl_email`].

pub mod processor;
pub mod render;
