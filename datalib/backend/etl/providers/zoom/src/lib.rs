//! The `zoom` provider: one Zoom user's meetings, with each cloud
//! recording's transcript and small files in the blob CAS, who attended,
//! and the AI Companion's summaries. The meetings render in
//! `datalib_etl_zoom_render`; INGEST.md says what is fetched and why.

pub mod ingest;
pub mod probe;
pub mod processor;
