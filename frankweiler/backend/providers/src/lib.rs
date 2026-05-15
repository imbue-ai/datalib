//! Frankweiler provider crate: per-service downloaders + ingest.
//!
//! Currently hosts the Slack downloader; other providers (anthropic,
//! openai, github, gitlab, notion) remain Python until M2+.

pub mod event_store;
pub mod obs;
pub mod slack;
