//! The render half of the `todoist` provider: one document per project,
//! its sections, tasks and comments, and their `grid_rows`. The download
//! half is [`datalib_etl_todoist`]; what becomes what is TRANSLATE.md.

pub mod ids;
pub mod processor;
pub mod render;
pub mod store;
