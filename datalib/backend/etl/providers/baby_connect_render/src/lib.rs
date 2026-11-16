//! The render half of the `baby_connect` provider: one document per child
//! per month, every entry logged for the child that month with its note,
//! and their `grid_rows`. The download half is
//! [`datalib_etl_baby_connect`]; what becomes what is TRANSLATE.md.

pub mod categories;
pub mod ids;
pub mod processor;
pub mod render;
pub mod store;
