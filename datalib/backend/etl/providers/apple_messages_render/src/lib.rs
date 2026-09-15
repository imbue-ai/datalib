//! The render half of the `apple_messages` provider: the mirrored
//! `chat.db` → markdown and `grid_rows`, through chat-common. The ingest
//! half is `datalib_etl_apple_messages`.

pub mod processor;
pub mod render;
pub mod typedstream;
