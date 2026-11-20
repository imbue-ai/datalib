//! The render half of the `markdown` provider: raw store -> one document
//! and one grid row per note. The download half is
//! [`datalib_etl_markdown`].

pub mod processor;
pub mod render;
