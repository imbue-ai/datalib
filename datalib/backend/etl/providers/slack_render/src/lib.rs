//! The render half of the `slack` provider: raw store -> markdown
//! and `grid_rows`. The download half is [`datalib_etl_slack`].

pub mod processor;
pub mod render;
