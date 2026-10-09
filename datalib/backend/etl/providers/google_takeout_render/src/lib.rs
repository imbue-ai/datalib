//! The render half of the `google_takeout` provider: raw store -> markdown
//! and `grid_rows`. The download half is [`datalib_etl_google_takeout`].

pub mod feeds;
pub mod gemini;
pub mod ids;
pub mod maps;
pub mod processor;
pub mod render;
pub mod youtube;
