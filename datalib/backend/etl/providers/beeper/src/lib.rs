//! The download half of the `beeper` source: Beeper Texts' own SQLite
//! files mirrored table for table, and its cached media copied into the
//! blob store. What the rows mean is `datalib_etl_beeper_render`'s
//! business; `INGEST.md` says what is read and why.

pub mod ingest;
pub mod processor;
