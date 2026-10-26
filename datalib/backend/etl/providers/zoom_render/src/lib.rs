//! The render half of the `zoom` provider: each meeting as one document
//! through chat-common, with its `grid_rows`. The download half is
//! [`datalib_etl_zoom`]; what becomes what is TRANSLATE.md.

pub mod ids;
pub mod processor;
pub mod render;
pub mod store;
pub mod vtt;
