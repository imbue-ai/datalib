//! One ingest run of the mirror, as a provider's processor and its
//! `*-ingest` binary drive it.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use sqlx::sqlite::SqlitePool;
use tracing::info;

use datalib_etl::doltlite_raw as dr;
use datalib_etl::progress::{Progress, TracingSink};

use crate::mirror::{self, MirrorOptions, MirrorStats};

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

/// Ingest the source file into the mirror. Does not commit; see
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

pub async fn fetch_and_commit(
    db: &Path,
    options: MirrorOptions,
    source: &str,
    started: Instant,
) -> Result<()> {
    let pool = mirror::open_mirror(db).await?;
    let stats = fetch(FetchOptions {
        mirror_path: db.to_path_buf(),
        pool: Some(pool.clone()),
        options,
        progress: Progress::new(Arc::new(TracingSink::new(source))),
    })
    .await?;

    let summary = stats.summary();
    let commit = dr::commit_run(&pool, &format!("{source}: {summary}")).await?;
    pool.close().await;

    match &commit {
        Some(hash) => info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            commit = %hash,
            "{summary}"
        ),
        // An unchanged source file rewrites every row and still produces
        // no commit, because every row hashes to the chunk already at HEAD.
        None => info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            "{summary} (no changes since last run)"
        ),
    }
    Ok(())
}
