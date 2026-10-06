//! Download (ingest) side of the `lightroom` source: the shared
//! SQLite→doltlite mirror engine, pointed at a `.lrcat`, a backup `.zip`,
//! or a folder of backups.

pub mod backups;
pub mod sync;
pub mod unpack;

pub use datalib_etl_sqlite_mirror::{
    fetch, fetch_and_commit, mirror, FetchOptions, MirrorOptions, MirrorStats,
};
