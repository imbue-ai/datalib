//! `fsindex` — directory-tree indexer CLI.
//!
//! Walks a local root, hashes everything visible, and lands the
//! result in a doltlite raw store. See the crate's `EXTRACT.md` for
//! the design.
//!
//! This binary is fsindex's own orchestrator: it opens the raw db,
//! runs `download::fetch` (which writes + gc's), and then issues the
//! single per-scan `dolt_commit`. Committing here (rather than inside
//! `fetch`) keeps the provider's download code commit-free per the
//! framework's commit-lifecycle rule, while still leaving a clean
//! working tree so the next open skips the rescue commit.

use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{Context, Result};
use clap::Parser;
use datalib_etl::control::DownloadControl;
use datalib_etl::fingerprint_cache::{self, FingerprintCache};
use datalib_etl::progress::Progress;
use datalib_etl_fsindex::download::{self, FetchOptions, RawDb};
use datalib_obs::{init as init_obs, ObsArgs};
use datalib_time::IsoOffsetTimestamp;
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

    /// Stable identifier for this scan source, stored as the
    /// `scan_meta.id` PK.
    ///
    /// An **id**, not a display name: nothing shows it to a person, and
    /// re-scanning the same source must reuse it or the upsert writes a
    /// second `scan_meta` row instead of updating the first.
    ///
    /// Defaults to the scan root's directory name, which is what you
    /// want standalone. The flag is here for the pipeline's sake: there
    /// a source's identity comes from its config entry
    /// (`PlanContext::name`), which is chosen once and deliberately
    /// outlives any particular path, so it cannot be derived from the
    /// root.
    ///
    /// `--source-name` is accepted as an alias; it was this flag's name
    /// when it was mandatory.
    #[arg(long, alias = "source-name")]
    source_id: Option<String>,

    /// Directory root to scan.
    #[arg(long)]
    root: PathBuf,

    /// Where this host keeps its fingerprint cache.
    ///
    /// The cache is what makes a rescan fast: it remembers each path's
    /// `(mtime, size, inode, dev)` and the hash that went with them, so
    /// an unchanged file is never re-read. It is host state and
    /// deliberately *not* part of the scan store — inode numbers mean
    /// nothing on another machine, a data root may be synced between
    /// machines, and a fresh branch of the scan data should not cost
    /// you a full rehash.
    ///
    /// Defaults to this machine's cache directory
    /// (`$DATALIB_CACHE_DIR`, else `$XDG_CACHE_HOME/datalib`, else
    /// `~/Library/Caches/datalib` or `~/.cache/datalib`). Deleting it
    /// is always safe: the next scan rebuilds it, slowly.
    #[arg(long)]
    cache_db: Option<PathBuf>,

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

    // Resolve the source name before anything else touches the db, so
    // a bad root fails here rather than after opening a store.
    let source_id = match args.source_id.clone() {
        Some(id) => id,
        None => default_source_id(&args.root)?,
    };

    let started = Instant::now();
    // Wall-clock start, in our canonical offset-bearing ISO format, for
    // the commit-message provenance block below.
    let started_at = IsoOffsetTimestamp::now_local().to_rfc3339();

    // Open the db ourselves so we can issue the single end-of-scan
    // commit after `fetch` returns.
    let db = RawDb::open(&args.db).await?;
    let cache_path = match args.cache_db.clone() {
        Some(p) => p,
        None => fingerprint_cache::default_cache_path()?,
    };
    let cache = FingerprintCache::open(&cache_path).await?;
    info!(
        event = "fsindex_cache_open",
        path = %cache_path.display(),
        entries = cache.count().await.unwrap_or(-1),
        "using this host's fingerprint cache",
    );
    // Live terminal bar attached to obs's shared MultiProgress (same
    // wiring the pipeline gives each source). Falls back to
    // tracing-only when obs::init didn't publish a MultiProgress. Held
    // here so we can stamp a final summary line on it after the scan.
    let progress = Progress::indicatif_message_only(source_id.clone());
    let opts = FetchOptions {
        db_path: args.db.clone(),
        db: Some(db.clone()),
        source_id: source_id.clone(),
        root: args.root.clone(),
        target_doltlite_branch: args.branch.clone(),
        cache,
        no_stamp: args.no_stamp,
        progress: progress.clone(),
        control: DownloadControl {
            reset_and_redownload: args.reset,
            ..Default::default()
        },
    };

    let summary = download::fetch(opts).await?;
    progress.finish(&format!(
        "done — scanned {}: {} files cached, {} files hashed ({}), {} dirs, {} symlinks, {} errors",
        summary.entries_scanned,
        summary.files_reused,
        summary.files_hashed,
        download::human_bytes(summary.bytes_hashed),
        summary.dirs,
        summary.symlinks,
        summary.errors,
    ));

    // Orchestrator tail: commit THEN gc, in that order. `dolt_commit`
    // first seals the working set into one `dolt_log` entry (and leaves
    // a clean tree so the next open skips the rescue commit); `dolt_gc`
    // then reclaims the per-batch chunk novelty against the committed
    // tree. The reverse order (gc-then-commit on one connection) fails
    // with "failed to flush" at scale — see `download::fetch`.
    let finished_at = IsoOffsetTimestamp::now_local().to_rfc3339();
    let scan_secs = started.elapsed().as_secs_f64();
    let commit_ms = db
        .commit(&commit_message(
            &source_id,
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
        files_reused = summary.files_reused,
        files_hashed = summary.files_hashed,
        dirs = summary.dirs,
        symlinks = summary.symlinks,
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
            "fsindex: scanned={} files_reused={} files_hashed={} dirs={} symlinks={} \
             stamped={} errors={} hashed={} skipped={} wall={:.2}s",
            summary.entries_scanned,
            summary.files_reused,
            summary.files_hashed,
            summary.dirs,
            summary.symlinks,
            summary.stamped_directories,
            summary.errors,
            download::human_bytes(summary.bytes_hashed),
            download::human_bytes(summary.bytes_skipped),
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
/// The scan root's own directory name, used when `--source-id` is not
/// given.
///
/// The root is canonicalized first so `.`, `..` and a trailing slash
/// all resolve to a real directory name rather than to an empty or
/// misleading one. A root that *is* the filesystem root has no name to
/// take, so it falls back to `root`.
fn default_source_id(root: &Path) -> Result<String> {
    let canonical = root
        .canonicalize()
        .with_context(|| format!("resolve scan root {}", root.display()))?;
    Ok(canonical
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "root".to_string()))
}

fn commit_message(
    source_id: &str,
    root: &str,
    started_at: &str,
    finished_at: &str,
    scan_secs: f64,
    summary: &download::FetchSummary,
) -> String {
    format!(
        "fsindex {source}: {scanned} entries, hashed {files_hashed} files ({hashed}), \
         reused {files_reused}\n\
         \n\
         host: {host}\n\
         user: {user}\n\
         root: {root}\n\
         started: {started_at}\n\
         finished: {finished_at}\n\
         duration: {scan_secs:.2}s\n\
         scanned: {scanned} (= {files_reused} files reused + {files_hashed} files hashed \
         + {dirs} dirs + {symlinks} symlinks)\n\
         hashed: {hashed} across {files_hashed} files\n\
         skipped: {skipped} (reused from rescan cursor)\n\
         stamped_dirs: {stamped}\n\
         errors: {errors}\n",
        source = source_id,
        scanned = summary.entries_scanned,
        files_reused = summary.files_reused,
        files_hashed = summary.files_hashed,
        dirs = summary.dirs,
        symlinks = summary.symlinks,
        hashed = download::human_bytes(summary.bytes_hashed),
        skipped = download::human_bytes(summary.bytes_skipped),
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

#[cfg(test)]
mod tests {
    use super::default_source_id;
    use std::path::Path;

    #[test]
    fn takes_the_root_directory_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("my_photos");
        std::fs::create_dir(&root).unwrap();
        assert_eq!(default_source_id(&root).unwrap(), "my_photos");
    }

    #[test]
    fn a_trailing_slash_or_dot_dot_resolves_to_the_same_name() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("my_photos");
        std::fs::create_dir_all(root.join("sub")).unwrap();
        let plain = default_source_id(&root).unwrap();
        let slashed = default_source_id(&root.join("")).unwrap();
        let dotdot = default_source_id(&root.join("sub").join("..")).unwrap();
        assert_eq!(plain, "my_photos");
        assert_eq!(slashed, plain, "a trailing slash changed the name");
        assert_eq!(dotdot, plain, "`..` changed the name");
    }

    #[test]
    fn the_filesystem_root_has_no_name_to_take() {
        // `/` canonicalizes to itself and has no final component, so
        // there is nothing to name the scan after.
        assert_eq!(default_source_id(Path::new("/")).unwrap(), "root");
    }

    #[test]
    fn a_root_that_does_not_exist_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("nope");
        let err = default_source_id(&missing).unwrap_err();
        assert!(
            format!("{err}").contains("resolve scan root"),
            "unhelpful error: {err}"
        );
    }
}
