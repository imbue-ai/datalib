//! `datalib_etl_sqlite_mirror` — the SQLite→doltlite mirror engine
//! behind every source whose data is a SQLite file the application
//! keeps for itself (`lightroom`, `apple_photos`). Drops and refills
//! every table each run and lets doltlite's content-addressed storage
//! turn that into an incremental, versioned backup. A provider crate
//! adds the path to the file, its defaults, and nothing else.

pub mod mirror;
pub mod plan;

pub use mirror::{open_mirror, open_sqlite, run, snapshot, MirrorOptions, MirrorStats, Snapshot};
pub use plan::{KeyOrigin, TableKind};
