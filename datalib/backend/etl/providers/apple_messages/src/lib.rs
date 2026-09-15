//! Ingest side of the `apple_messages` source: the shared SQLite→doltlite
//! mirror engine, pointed at Messages' `chat.db`. Rendering lives in
//! `datalib_etl_apple_messages_render`.

pub mod processor;
