//! The render half of the `google_takeout` provider: raw store -> markdown
//! and `grid_rows`. The download half is [`datalib_etl_google_takeout`].

pub mod processor;
pub mod render;
