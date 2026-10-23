//! The render half of the `google_drive` provider: each Google Doc's
//! newest capture as markdown, its comment threads folded in, and its
//! `grid_rows`. Sheets and Slides do not render yet. The download half
//! is [`datalib_etl_google_drive`]; what becomes what is TRANSLATE.md.

pub mod comments;
pub mod doc;
pub mod ids;
pub mod processor;
pub mod render;
pub mod store;
pub mod unsupported;
