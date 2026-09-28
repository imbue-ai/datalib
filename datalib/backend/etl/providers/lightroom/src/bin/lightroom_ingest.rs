//! `lightroom-ingest` — mirror a Lightroom catalog into a doltlite store.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use clap::Parser;
use datalib_etl_lightroom::ingest::{self, MirrorOptions};
use datalib_etl_lightroom_config::XMP_COLUMN_PATTERNS;
use datalib_obs::{init as init_obs, ObsArgs};

#[derive(Parser, Debug)]
#[command(
    name = "lightroom-ingest",
    about = "Mirror a Lightroom catalog (or any SQLite file) into a doltlite store, \
             so repeated runs form a deduplicated, versioned backup."
)]
struct Args {
    /// The catalog to mirror. Any SQLite database works.
    #[arg(long)]
    catalog: PathBuf,

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
    let options = MirrorOptions {
        source_path: args.catalog.clone(),
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
        primary_keys: BTreeMap::new(),
        gc: args.gc,
        sidecar_tables: Vec::new(),
    };

    ingest::fetch_and_commit(&args.db, options, "lightroom", started).await
}
