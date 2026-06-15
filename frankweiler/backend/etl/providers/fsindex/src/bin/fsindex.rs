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
use frankweiler_time::IsoOffsetTimestamp;
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
    // Wall-clock start, in our canonical offset-bearing ISO format, for
    // the commit-message provenance block below.
    let started_at = IsoOffsetTimestamp::now_local().to_rfc3339();

    // Open the db ourselves so we can issue the single end-of-scan
    // commit after `fetch` returns.
    let db = RawDb::open(&args.db).await?;
    // Live terminal bar attached to obs's shared MultiProgress (same
    // wiring frankweiler-sync gives each source). Falls back to
    // tracing-only when obs::init didn't publish a MultiProgress. Held
    // here so we can stamp a final summary line on it after the scan.
    let progress = Progress::indicatif_message_only(args.source_name.clone());
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
    let finished_at = IsoOffsetTimestamp::now_local().to_rfc3339();
    let scan_secs = started.elapsed().as_secs_f64();
    let commit_ms = db
        .commit(&commit_message(
            &args.source_name,
            &args.root.display().to_string(),
            &started_at,
            &finished_at,
            scan_secs,
            &summary,
        ))
        .await?
        .as_secs_f64()
        * 1000.0;
    // What did this scan actually change, vs the last committed scan?
    // Read straight from the dolt diff now that the commit has landed.
    // Best-effort: the first scan has no parent to diff against.
    if let Some(diff) = db.diff_counts_since_parent().await {
        let unchanged = (summary.entries_scanned as u64).saturating_sub(diff.added + diff.modified);
        info!(
            event = "fsindex_diff_summary",
            added = diff.added,
            modified = diff.modified,
            removed = diff.removed,
            unchanged = unchanged,
            "vs last scan: {} added, {} modified, {} removed, {} unchanged",
            diff.added,
            diff.modified,
            diff.removed,
            unchanged,
        );
    }

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
        bytes_hashed = summary.bytes_hashed,
        bytes_skipped = summary.bytes_skipped,
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
            "fsindex: scanned={} rehashed={} reused={} stamped={} errors={} \
             hashed={} skipped={} wall={:.2}s",
            summary.entries_scanned,
            summary.entries_rehashed,
            summary.entries_reused,
            summary.stamped_directories,
            summary.errors,
            extract::human_bytes(summary.bytes_hashed),
            extract::human_bytes(summary.bytes_skipped),
            elapsed.as_secs_f64(),
        );
    }
    Ok(())
}

/// Build the `dolt_commit` message: a one-line subject plus a provenance
/// and stats body, so `dolt log` alone answers "who scanned what, when,
/// from where, and how much moved." Diff counts (added/modified/etc.)
/// are deliberately absent — they're only computable *after* this commit
/// exists, so they live in the post-commit `fsindex_diff_summary` log.
fn commit_message(
    source_name: &str,
    root: &str,
    started_at: &str,
    finished_at: &str,
    scan_secs: f64,
    summary: &extract::FetchSummary,
) -> String {
    format!(
        "fsindex {source}: {scanned} entries, hashed {hashed} ({rehashed} files), \
         reused {reused}\n\
         \n\
         host: {host}\n\
         user: {user}\n\
         root: {root}\n\
         started: {started_at}\n\
         finished: {finished_at}\n\
         duration: {scan_secs:.2}s\n\
         scanned: {scanned} (rehashed {rehashed}, reused {reused})\n\
         hashed: {hashed} across {rehashed} files\n\
         skipped: {skipped} (reused from rescan cursor)\n\
         stamped_dirs: {stamped}\n\
         errors: {errors}\n",
        source = source_name,
        scanned = summary.entries_scanned,
        rehashed = summary.entries_rehashed,
        reused = summary.entries_reused,
        hashed = extract::human_bytes(summary.bytes_hashed),
        skipped = extract::human_bytes(summary.bytes_skipped),
        stamped = summary.stamped_directories,
        errors = summary.errors,
        host = hostname(),
        user = username(),
        root = root,
        started_at = started_at,
        finished_at = finished_at,
        scan_secs = scan_secs,
    )
}

/// Best-effort hostname. No std API, so shell out to `hostname` (on
/// PATH across macOS/Linux/Windows, same as this codebase already shells
/// out to `dolt`/`sqlite3`/`latchkey`). Falls back to `unknown`.
fn hostname() -> String {
    std::process::Command::new("hostname")
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Best-effort username from the environment (`USER` on unix/macOS,
/// `USERNAME` on Windows). Falls back to `unknown`.
fn username() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string())
}
