//! The `zoom` provider: one Zoom user's meetings, who attended, and the
//! AI Companion's summaries. No recording file is downloaded. The meetings render in
//! `datalib_etl_zoom_render`; INGEST.md says what is fetched and why.

pub mod ingest;
pub mod probe;
pub mod processor;
