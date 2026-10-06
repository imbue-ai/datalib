//! Download (ingest) side of the `apple_photos` source: the shared
//! SQLite→doltlite mirror engine, pointed at a library's `Photos.sqlite`.

pub use datalib_etl_sqlite_mirror::{
    fetch, fetch_and_commit, mirror, FetchOptions, MirrorOptions, MirrorStats,
};
