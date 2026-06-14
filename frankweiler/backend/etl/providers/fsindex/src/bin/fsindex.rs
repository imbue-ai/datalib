//! `fsindex` — directory-tree indexer CLI.
//!
//! Walks a local root, hashes everything visible, and lands the
//! result in a doltlite raw store. See the crate's `EXTRACT.md` for
//! the design.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use clap::Parser;
use frankweiler_etl::control::ExtractControl;
use frankweiler_etl::progress::Progress;
use frankweiler_etl_fsindex::extract::{self, FetchOptions};
use frankweiler_obs::{init as init_obs, ObsArgs};
use tracing::info;

#[derive(Parser, Debug)]
#[command(
    name = "fsindex",
    about = "Scan a directory tree and record (path, kind, size, blake3) per entry into a doltlite db."
)]
struct Args {
    /// Output doltlite db path. The file is created if missing.
    #[arg(long)]
    db: PathBuf,

    /// Stable identifier for this scan source. Used as the
    /// `scan_meta.id` PK and as the row identity if multiple scan
    /// roots share one db (via `--branch`).
    #[arg(long)]
    source_name: String,

    /// Directory root to scan.
    #[arg(long)]
    root: PathBuf,

    /// Doltlite branch to write into. Defaults to whatever branch the
    /// db is currently on (`main` on first open).
    #[arg(long)]
    branch: Option<String>,

    /// Disable identity-UUID breadcrumb stamping, regardless of
    /// `.fsindex.yaml` config. The scanner is read-only when set.
    #[arg(long)]
    no_stamp: bool,

    /// Truncate the data + bookkeeping tables before scanning. The
    /// next run starts from an empty cache so every entry rehashes.
    #[arg(long)]
    reset: bool,

    #[command(flatten)]
    obs: ObsArgs,
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> Result<()> {
    let args = Args::parse();
    let _guard = init_obs(&args.obs, "fsindex")?;

    let started = Instant::now();
    let opts = FetchOptions {
        db_path: args.db.clone(),
        db: None,
        source_name: args.source_name.clone(),
        root: args.root.clone(),
        target_doltlite_branch: args.branch.clone(),
        no_stamp: args.no_stamp,
        progress: Progress::default(),
        control: ExtractControl {
            reset_and_redownload: args.reset,
            ..Default::default()
        },
    };

    let summary = extract::fetch(opts).await?;
    let elapsed = started.elapsed();

    info!(
        event = "fsindex_done",
        entries_scanned = summary.entries_scanned,
        entries_rehashed = summary.entries_rehashed,
        entries_reused = summary.entries_reused,
        stamped_directories = summary.stamped_directories,
        errors = summary.errors,
        wall_seconds = elapsed.as_secs_f64(),
    );
    // CLI summary to stdout: this binary is a pipe-friendly tool, so a
    // one-line machine-greppable summary on stdout is intentional (the
    // structured event above goes to the stderr log sink).
    #[allow(clippy::disallowed_macros)]
    {
        println!(
            "fsindex: scanned={} rehashed={} reused={} stamped={} errors={} wall={:.2}s",
            summary.entries_scanned,
            summary.entries_rehashed,
            summary.entries_reused,
            summary.stamped_directories,
            summary.errors,
            elapsed.as_secs_f64(),
        );
    }
    Ok(())
}
