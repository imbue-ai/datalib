//! Google Takeout provider for [`frankweiler_etl`]: walks a Google
//! Takeout export tree on disk (`~/backups/Takeout/` or similar) and
//! lands the entries we care about — Maps reviews / saved places /
//! photos, YouTube watch history + subscriptions, Google Chat
//! DMs + bots + attachments, and Gemini Apps activity — into a
//! provider-owned doltlite raw store.
//!
//! See `docs/dev/google_takeout_ingestion.md` for the design.
//!
//! One provider, many feeds: each top-level Takeout slice gets its
//! own walker module; the YAML `sync:` block lets users opt feeds in
//! individually. All feeds share the same input root, the same lack
//! of upstream auth, and the same shared-helpers write path
//! (`frankweiler_etl::bulk` + `frankweiler_etl::blob_cas`) — the
//! splits would have multiplied wiring without separating any logic
//! worth separating.
//!
//! ## Scope (first pass)
//!
//! - Raw-download only. No render, no `GridRow`s yet — see
//!   `docs/dev/google_takeout_ingestion.md` § "Out of scope (first
//!   pass)".
//! - Wire-tape JSONL is not emitted (the data didn't come off a
//!   wire).
//! - The HTML feeds (`youtube_watch_history`, `gemini_apps`) parse
//!   Google's MDL-styled cell-per-entry export into structured JSON
//!   in the `payload` column; the full upstream HTML file still
//!   lives on disk at `input_path` if anyone wants the rendered
//!   form.

pub mod download;
pub mod processor;
pub mod render;
