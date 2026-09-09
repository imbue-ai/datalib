//! The render half of the `chatgpt` provider: raw store -> markdown
//! and `grid_rows`. The download half is [`datalib_etl_chatgpt`].

pub mod processor;
pub mod render;
