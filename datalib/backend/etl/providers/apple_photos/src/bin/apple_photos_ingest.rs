//! `apple-photos-ingest` — mirror an Apple Photos library's database into
//! a doltlite store.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use clap::Parser;
use datalib_etl::doltlite_raw as dr;
use datalib_etl::progress::{Progress, TracingSink};
use datalib_etl_apple_photos::ingest::{self, mirror, FetchOptions};
use datalib_etl_apple_photos::processor::mirror_options;
use datalib_etl_apple_photos_config::ApplePhotosConfig;
use datalib_obs::{init as init_obs, ObsArgs};
use datalib_source_common::LocalPath;
use tracing::info;

#[derive(Parser, Debug)]
#[command(
    name = "apple-photos-ingest",
    about = "Mirror an Apple Photos library (its database/Photos.sqlite) into a doltlite \
             store, so repeated runs form a deduplicated, versioned backup."
)]
struct Args {
    /// The `.photoslibrary` bundle, or its `database/Photos.sqlite`.
    #[arg(long)]
    library: PathBuf,

    /// Output doltlite db path. Created if missing.
    #[arg(long)]
    db: PathBuf,

    /// Mirror Core Data's persistent history and the daemons' work queue
    /// too. Off by default: they change on every run whether or not a
    /// photo did.
    #[arg(long)]
    keep_history: bool,

    /// Extra `Table.column` globs to drop.
    #[arg(long = "exclude-column", value_name = "GLOB")]
    exclude_columns: Vec<String>,

    /// Table-name globs to skip.
    #[arg(long = "exclude-table", value_name = "GLOB")]
    exclude_tables: Vec<String>,

    /// Table-name globs to mirror (default: everything).
    #[arg(long = "include-table", value_name = "GLOB")]
    include_tables: Vec<String>,

    /// Mirror each table's declared primary key (`Z_PK`) verbatim instead
    /// of preferring a stable `ZUUID` column. See `INGEST.md`.
    #[arg(long)]
    declared_keys: bool,

    /// Read the library file in place instead of taking a `VACUUM INTO`
    /// snapshot first. Photos' daemons hold the file open at all times,
    /// so this reads a possibly half-written state.
    #[arg(long)]
    no_snapshot: bool,

    /// Collect unreachable chunks before mirroring. Much smaller store;
    /// history is unaffected. Costs a full rewrite of the chunk store.
    #[arg(long)]
    gc: bool,

    #[command(flatten)]
    obs: ObsArgs,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _guard = init_obs(&args.obs, "apple-photos-ingest")?;
    let started = Instant::now();

    let config = ApplePhotosConfig {
        library: Some(LocalPath {
            path: args.library.clone(),
        }),
        include_tables: if args.include_tables.is_empty() {
            vec!["*".to_string()]
        } else {
            args.include_tables.clone()
        },
        exclude_tables: args.exclude_tables.clone(),
        exclude_columns: args.exclude_columns.clone(),
        skip_history: !args.keep_history,
        stable_key_columns: if args.declared_keys {
            Vec::new()
        } else {
            vec!["ZUUID".to_string()]
        },
        snapshot: !args.no_snapshot,
        gc: args.gc,
        ..Default::default()
    };
    let options = mirror_options(&config)?;

    let pool = mirror::open_mirror(&args.db).await?;
    let stats = ingest::fetch(FetchOptions {
        mirror_path: args.db.clone(),
        pool: Some(pool.clone()),
        options,
        progress: Progress::new(std::sync::Arc::new(TracingSink::new("apple_photos"))),
    })
    .await?;

    let summary = stats.summary();
    let commit = dr::commit_run(&pool, &format!("apple_photos: {summary}")).await?;
    pool.close().await;

    match &commit {
        Some(hash) => info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            commit = %hash,
            "{summary}"
        ),
        // An unchanged library rewrites every row and still produces no
        // commit, because every row hashes to the chunk already at HEAD.
        None => info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            "{summary} (no changes since last run)"
        ),
    }
    Ok(())
}
