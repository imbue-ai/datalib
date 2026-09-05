//! Google Takeout provider for [`datalib_etl`]: walks a Google
//! Takeout export tree on disk (`~/backups/Takeout/` or similar) and
//! lands the entries we care about — Maps reviews / saved places /
//! photos, YouTube watch history + subscriptions, Google Chat
//! DMs + bots + attachments, and Gemini Apps activity — into a
//! provider-owned doltlite raw store.

pub mod download;
pub mod processor;
pub mod render;
