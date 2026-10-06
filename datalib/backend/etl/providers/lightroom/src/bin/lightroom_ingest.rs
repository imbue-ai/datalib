//! `lightroom-ingest` — mirror a Lightroom catalog, or a folder of its
//! backups, into a doltlite store.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::Result;
use clap::Parser;
use datalib_etl::doltlite_raw as dr;
use datalib_etl::fingerprint_cache::{self, FingerprintCache};
use datalib_etl::progress::{Progress, TracingSink};
use datalib_etl::stop::StopFlag;
use datalib_etl_lightroom::ingest::{mirror, sync, MirrorOptions};
use datalib_etl_lightroom_config::XMP_COLUMN_PATTERNS;
use datalib_obs::{init as init_obs, ObsArgs};

#[derive(Parser, Debug)]
#[command(
    name = "lightroom-ingest",
    about = "Mirror a Lightroom catalog (or any SQLite file) into a doltlite store, \
             so repeated runs form a deduplicated, versioned backup."
)]
#[command(group = clap::ArgGroup::new("input").required(true).multiple(true).args(["catalog", "backups"]))]
struct Args {
    /// The catalog to mirror, or a backup `.zip` holding one. Any SQLite
    /// database works.
    #[arg(long)]
    catalog: Option<PathBuf>,

    /// A folder of Lightroom backups. Each one the store does not have
    /// becomes a commit, oldest first, dated when it was taken; with
    /// `--catalog` too, the catalog is mirrored on top.
    #[arg(long)]
    backups: Option<PathBuf>,

    /// Output doltlite db path. Created if missing.
    #[arg(long)]
    db: PathBuf,

    /// Drop the bulky derived metadata columns (the per-image XMP packet
    /// and the flattened search indexes). Smaller backup, no loss of
    /// information that isn't reconstructible.
    #[arg(long)]
    skip_xmp: bool,

    /// Extra `Table.column` globs to drop.
    #[arg(long = "exclude-column", value_name = "GLOB")]
    exclude_columns: Vec<String>,

    /// Table-name globs to skip.
    #[arg(long = "exclude-table", value_name = "GLOB")]
    exclude_tables: Vec<String>,

    /// Table-name globs to mirror (default: everything).
    #[arg(long = "include-table", value_name = "GLOB")]
    include_tables: Vec<String>,

    /// Mirror each table's declared primary key verbatim instead of
    /// preferring a stable `id_global` UNIQUE column. See `INGEST.md`
    /// §"When the primary key changes".
    #[arg(long)]
    declared_keys: bool,

    /// Read the catalog file in place instead of taking a `VACUUM INTO`
    /// snapshot first. Faster, but unsafe while Lightroom is running.
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
    let _guard = init_obs(&args.obs, "lightroom-ingest")?;
    let started = Instant::now();

    let mut exclude_columns = args.exclude_columns.clone();
    if args.skip_xmp {
        exclude_columns.extend(XMP_COLUMN_PATTERNS.iter().map(|s| s.to_string()));
    }
    let input = args
        .catalog
        .clone()
        .or(args.backups.clone())
        .expect("clap requires one");
    let options = MirrorOptions {
        snapshot: !args.no_snapshot,
        include_tables: if args.include_tables.is_empty() {
            vec!["*".to_string()]
        } else {
            args.include_tables.clone()
        },
        exclude_tables: args.exclude_tables.clone(),
        exclude_columns,
        stable_key_columns: if args.declared_keys {
            Vec::new()
        } else {
            vec!["id_global".to_string()]
        },
        gc: args.gc,
        ..MirrorOptions::new(input)
    };

    let progress = Progress::new(Arc::new(TracingSink::new("lightroom")));
    let pool = mirror::open_mirror(&args.db).await?;
    let cache = FingerprintCache::open(&fingerprint_cache::default_cache_path()?).await?;
    let run = sync::run(
        &pool,
        &cache,
        sync::Inputs {
            backups: args.backups.as_deref(),
            catalog: args.catalog.as_deref(),
        },
        &options,
        &progress,
        &StopFlag::default(),
        "lightroom",
    )
    .await?;
    let summary = run.summary();
    let commit = dr::commit_run(&pool, &format!("download lightroom: {summary}")).await?;
    pool.close().await;
    let elapsed_ms = started.elapsed().as_millis() as u64;
    match commit {
        Some(hash) => tracing::info!(elapsed_ms, commit = %hash, "{summary}"),
        None => tracing::info!(elapsed_ms, "{summary} (no further changes)"),
    }
    Ok(())
}
