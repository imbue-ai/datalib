//! `datalib_etl_sqlite_mirror` — the SQLite→doltlite mirror engine
//! behind every source whose data is a SQLite file the application
//! keeps for itself (`lightroom`, `apple_photos`, `apple_messages`,
//! `whatsapp`). Drops and
//! refills every table each run and lets doltlite's content-addressed
//! storage turn that into an incremental, versioned backup; a source that
//! evicts is upserted instead, keeping what it let go
//! (`MirrorOptions::append_only`). A source with no table in it is
//! refused before anything is dropped (`NothingToMirror`). A provider
//! crate adds the path to the file and its defaults; `whatsapp` also
//! decrypts first and keeps a media registry beside the mirror
//! (`MirrorOptions::sidecar_tables`).

pub mod ingest;
pub mod mirror;
pub mod plan;

pub use ingest::{fetch, fetch_and_commit, FetchOptions};
pub use mirror::{
    open_mirror, open_sqlite, run, run_or_report, snapshot, MirrorOptions, MirrorStats,
    NothingToMirror, Snapshot,
};
pub use plan::{KeyOrigin, TableKind};
