//! The `google_drive` provider: a Google account's Docs, Sheets and
//! Slides, each kept in Google's native form at every version a run saw,
//! with its comments, revisions and exports. The Docs render in
//! `datalib_etl_google_drive_render`; the plan is
//! docs/dev/plans/google_drive.md.

pub mod ingest;
pub mod processor;
