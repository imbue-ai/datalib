//! Frankweiler provider crate: per-service downloaders + ingest.
//!
//! Currently hosts the Slack downloader; other providers (anthropic,
//! openai, github, gitlab, notion) remain Python until M2+.

pub mod grid_rows_load;
pub mod obs;
pub mod raw_store;
pub mod slack;
