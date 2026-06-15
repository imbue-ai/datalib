//! Indicatif-backed progress wiring for sync.
//!
//! The implementation now lives in
//! [`frankweiler_etl::indicatif_progress`] so the standalone provider
//! CLIs (`fsindex`, the `<provider>_download` bins) render the exact
//! same bar as the orchestrator. This module re-exports the pieces
//! sync uses directly — it manages one `MultiProgress` across many
//! per-source bars itself, so it wants the building blocks rather than
//! the [`frankweiler_etl::progress::Progress::indicatif`] one-liner.
//!
//! Sync creates one `MultiProgress` for the run (via [`make_multi`])
//! and one [`make_bar`] per managed source; each bar is wrapped in an
//! [`IndicatifSink`] and fanned out with a `TracingSink` so every
//! emission point drives both the terminal UI and the structured event
//! stream. Inner bars (per-channel within a Slack source, say) are
//! spawned on demand via `Progress::child(prefix)`.

pub use frankweiler_etl::indicatif_progress::{make_bar, make_multi, IndicatifSink};
