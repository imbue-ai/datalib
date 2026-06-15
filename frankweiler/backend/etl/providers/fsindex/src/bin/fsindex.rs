//! `fsindex` — directory-tree indexer CLI.
//!
//! Walks a local root, hashes everything visible, and lands the
//! result in a doltlite raw store. See the crate's `EXTRACT.md` for
//! the design.
//!
//! This binary is fsindex's own orchestrator: it opens the raw db,
//! runs `extract::fetch` (which writes + gc's), and then issues the
//! single per-scan `dolt_commit`. Committing here (rather than inside
//! `fetch`) keeps the provider's extract code commit-free per the
//! framework's commit-lifecycle rule, while still leaving a clean
//! working tree so the next open skips the rescue commit.

use std::path::PathBuf;
use std::time::Instant;

use anyhow::Result;
use clap::Parser;
use frankweiler_etl::control::ExtractControl;
use frankweiler_etl::progress::Progress;
use frankweiler_etl_fsindex::extract::{self, FetchOptions, RawDb};
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

    // Open the db ourselves so we can issue the single end-of-scan
    // commit after `fetch` returns.
    let db = RawDb::open(&args.db).await?;
    // Live terminal bar attached to obs's shared MultiProgress (same
    // wiring frankweiler-sync gives each source). Falls back to
    // tracing-only when obs::init didn't publish a MultiProgress. Held
    // here so we can stamp a final summary line on it after the scan.
    let progress = Progress::indicatif(args.source_name.clone());
    let opts = FetchOptions {
        db_path: args.db.clone(),
        db: Some(db.clone()),
        source_name: args.source_name.clone(),
        root: args.root.clone(),
        target_doltlite_branch: args.branch.clone(),
        no_stamp: args.no_stamp,
        progress: progress.clone(),
        control: ExtractControl {
            reset_and_redownload: args.reset,
            ..Default::default()
        },
    };

    let summary = extract::fetch(opts).await?;
    progress.finish(&format!(
        "done — scanned {} (reused {}, rehashed {}, errors {})",
        summary.entries_scanned, summary.entries_reused, summary.entries_rehashed, summary.errors,
    ));

    // Orchestrator tail: commit THEN gc, in that order. `dolt_commit`
    // first seals the working set into one `dolt_log` entry (and leaves
    // a clean tree so the next open skips the rescue commit); `dolt_gc`
    // then reclaims the per-batch chunk novelty against the committed
    // tree. The reverse order (gc-then-commit on one connection) fails
    // with "failed to flush" at scale — see `extract::fetch`.
    let commit_ms = db
        .commit(&format!(
            "fsindex {}: scanned={} reused={} rehashed={} errors={}",
            args.source_name,
            summary.entries_scanned,
            summary.entries_reused,
            summary.entries_rehashed,
            summary.errors,
        ))
        .await?
        .as_secs_f64()
        * 1000.0;
    // gc is best-effort: a successful scan + commit is the durable
    // result. dolt_gc can fail (e.g. "gc sweep phase failed") when the
    // un-compacted store is very large relative to free disk; that
    // leaves a bigger-than-ideal db but does not lose data, so we warn
    // rather than fail the run.
    let gc_ms = match db.gc().await {
        Ok(d) => d.as_secs_f64() * 1000.0,
        Err(e) => {
            tracing::warn!(event = "fsindex_gc_failed", error = %format!("{e:#}"));
            -1.0
        }
    };

    let elapsed = started.elapsed();
    info!(
        event = "fsindex_done",
        entries_scanned = summary.entries_scanned,
        entries_rehashed = summary.entries_rehashed,
        entries_reused = summary.entries_reused,
        stamped_directories = summary.stamped_directories,
        errors = summary.errors,
        commit_ms = commit_ms,
        gc_ms = gc_ms,
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
