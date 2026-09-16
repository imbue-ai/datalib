//! The render half of the `facebook` provider: raw store -> markdown
//! and `grid_rows`. The ingest half is [`datalib_etl_facebook`].

pub mod activity;
pub mod albums;
pub mod common;
pub mod friends;
pub mod posts;
pub mod processor;
