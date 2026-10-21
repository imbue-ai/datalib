//! The `google_drive` provider: a Google account's Docs, Sheets and
//! Slides, each kept in Google's native form at every version a run saw,
//! with its comments, revisions and exports. Download only; nothing
//! renders it yet (docs/dev/plans/google_drive.md).

pub mod ingest;
pub mod processor;
