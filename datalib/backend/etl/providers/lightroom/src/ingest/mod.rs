//! Download (ingest) side of the `lightroom` source: the shared
//! SQLite→doltlite mirror engine, pointed at a `.lrcat`.

use std::path::PathBuf;

use anyhow::Result;
use sqlx::sqlite::SqlitePool;

use datalib_etl::progress::Progress;

pub use datalib_etl_sqlite_mirror::{mirror, MirrorOptions, MirrorStats};

/// Everything one ingest run needs. Mirrors the shape of the other
/// providers' `FetchOptions`.
pub struct FetchOptions {
    /// The doltlite mirror store (`<raw_dir>/entities.doltlite_db`).
    /// Ignored when `pool` is `Some` — the orchestrator opens the store
    /// itself so it can register the interrupt-commit hook before any
    /// write happens.
    pub mirror_path: PathBuf,
    /// An already-open mirror pool to reuse.
    pub pool: Option<SqlitePool>,
    pub options: MirrorOptions,
    pub progress: Progress,
}

/// Ingest the catalog into the mirror. Does not commit; see
/// [`mirror::run`].
pub async fn fetch(opts: FetchOptions) -> Result<MirrorStats> {
    let owned;
    let pool = match &opts.pool {
        Some(p) => p,
        None => {
            owned = mirror::open_mirror(&opts.mirror_path).await?;
            &owned
        }
    };
    mirror::run(pool, &opts.options, &opts.progress).await
}
