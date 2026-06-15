//! `fsindex` extract entry point.
//!
//! Orchestrates: open DB, optional branch checkout, load Unison-style
//! rescan caches, truncate-and-rebuild, run the streaming walker as a
//! producer that pushes row batches over an mpsc channel to a writer
//! task, periodically emit progress + metrics, then write scan_meta.
//!
//! Stamping uses a legacy in-memory path (no producer-consumer) — it
//! mutates per-row `identity_uuid` after the walk so it can't stream
//! today. Toggle via `opts.no_stamp` or `--no-stamp` on the CLI.
//!
//! See [`docs/data_architecture_ingestion.md`](../../../../../docs/data_architecture_ingestion.md)
//! §"Commit lifecycle" — `fetch` returns and the caller decides
//! whether to `dolt_commit`.

pub mod db;
pub mod hash;
pub mod metrics;
pub mod options;
pub mod schema_raw;
pub mod stamp;
pub mod walker;

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use tokio::sync::mpsc;
use tracing::{info, warn};

use frankweiler_etl::control::ExtractControl;
use frankweiler_etl::progress::Progress;

pub use db::RawDb;

use metrics::WalkerCounters;
use options::{EffectiveOptions, FsindexYaml, Identity, OptionsCascade};
use schema_raw::{FileKind, FileRow, FileStatsRow, ScanMetaRow, StampKind};

/// One batch sent over the producer→consumer channel. files[i] and
/// stats[i] always match by id; they're emitted as a pair.
type Batch = (Vec<FileRow>, Vec<FileStatsRow>);

/// Bounded channel between the walker (producer) and the doltlite
/// writer (consumer). Small cap so backpressure shows up — if the
/// channel sits full, the writer is the bottleneck.
const BATCH_CHANNEL_CAPACITY: usize = 4;

/// Progress emission cadence. Cheap atomic loads, so doing this every
/// half second is essentially free; gives the user a continuous
/// feedback loop on long scans.
const PROGRESS_INTERVAL_MS: u64 = 500;

pub struct FetchOptions {
    pub db_path: PathBuf,
    pub db: Option<RawDb>,
    pub source_name: String,
    pub root: PathBuf,
    pub target_doltlite_branch: Option<String>,
    pub no_stamp: bool,
    pub progress: Progress,
    pub control: ExtractControl,
}

#[derive(Debug, Default, Clone)]
pub struct FetchSummary {
    pub entries_scanned: usize,
    pub entries_rehashed: usize,
    pub entries_reused: usize,
    pub stamped_directories: usize,
    pub errors: usize,
}

/// Run one extract pass against `opts.root`.
pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let total_start = Instant::now();
    let db = match opts.db.clone() {
        Some(db) => db,
        None => RawDb::open(&opts.db_path).await?,
    };
    if let Some(branch) = opts.target_doltlite_branch.as_deref() {
        db.checkout_branch(branch).await?;
    }
    let _ = opts.control.refetch_blobs;

    opts.progress
        .set_message(&format!("indexing {}", opts.root.display()));

    let load_start = Instant::now();
    let (prev_stats, prev_file_blake3s) = if opts.control.reset_and_redownload {
        (Default::default(), Default::default())
    } else {
        let cache = db.load_prev_cache().await?;
        info!(
            event = "fsindex_load_cache_done",
            file_rows = cache.stats.len(),
            elapsed_ms = load_start.elapsed().as_millis() as u64,
        );
        (cache.stats, cache.blake3s)
    };
    let phase_load = load_start.elapsed();

    let truncate_start = Instant::now();
    db.reset().await?;
    let phase_truncate = truncate_start.elapsed();

    let default_stamp_kind = if cfg!(unix) {
        StampKind::Inode
    } else {
        StampKind::NoStamp
    };

    let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();

    let (summary, walker_errors, counters, phase_walk, phase_write_total) = if opts.no_stamp {
        streaming_pipeline(
            opts.root.clone(),
            default_stamp_kind,
            prev_stats,
            prev_file_blake3s,
            db.clone(),
            now.clone(),
            opts.progress.clone(),
        )
        .await?
    } else {
        legacy_inmemory(
            &opts,
            default_stamp_kind,
            prev_stats,
            prev_file_blake3s,
            &db,
            &now,
        )
        .await?
    };

    // ── scan_meta ────────────────────────────────────────────────────
    let root_cascade = build_root_cascade(&opts.root);
    let effective = root_cascade.effective();
    let options_fp = options::options_fingerprint(&effective);
    let os = std::env::consts::OS.to_string();
    // FIXME(case_sensitive-heuristic): assume case-insensitive on
    // macOS default volumes, case-sensitive elsewhere.
    let case_sensitive = !matches!(os.as_str(), "macos");
    // FIXME(inode_stable-heuristic): assumed true for now.
    let inode_stable = true;
    let scan_meta = ScanMetaRow {
        id: opts.source_name.clone(),
        abs_path: opts.root.to_string_lossy().into_owned(),
        os,
        case_sensitive,
        inode_stable,
        options_fingerprint: options_fp,
        last_scan_at: now.clone(),
        scanner_version: env!("CARGO_PKG_VERSION").to_string(),
    };
    let scan_meta_start = Instant::now();
    db.write_scan_meta(&scan_meta, &now).await?;
    let phase_scan_meta = scan_meta_start.elapsed();

    // Walker errors (unreadable entries, non-utf8 names, …). fsindex
    // has no `_bookkeeping` sidecar to record them in — and there's no
    // retry model that would consult one. They're logged here and
    // counted in the `fsindex_phase_breakdown` event (`stat_errors`,
    // `read_errors`, `non_utf8_paths`), which is all the durable
    // evidence the scanner needs.
    for err in &walker_errors {
        warn!(event = "fsindex_entry_error", id = %err.id, error = %err.message);
    }

    // NB: the commit + gc happen in the ORCHESTRATOR (the standalone
    // binary), not here. Order is load-bearing: `dolt_commit` must run
    // BEFORE `dolt_gc` on a given connection. Running gc first and then
    // committing on the same sqlx connection fails with "failed to
    // flush" at scale (reproduced at 1M rows; fine at 100k). Committing
    // first records the working set; gc then reclaims the per-batch
    // chunk novelty against the committed tree. `fetch` stays
    // commit-free per the framework's commit-lifecycle rule.

    let total_elapsed = total_start.elapsed();

    // ── Final phase breakdown event ──────────────────────────────────
    let bytes_hashed = counters.bytes_hashed.load(Ordering::Relaxed);
    let bytes_saved = counters.bytes_skipped_cache.load(Ordering::Relaxed);
    let mb_per_s = if phase_walk.as_secs_f64() > 0.0 {
        (bytes_hashed as f64) / phase_walk.as_secs_f64() / 1_000_000.0
    } else {
        0.0
    };
    let entries_per_s = if phase_walk.as_secs_f64() > 0.0 {
        (counters.entries_total() as f64) / phase_walk.as_secs_f64()
    } else {
        0.0
    };
    info!(
        event = "fsindex_phase_breakdown",
        total_ms = total_elapsed.as_millis() as u64,
        load_caches_ms = phase_load.as_millis() as u64,
        truncate_ms = phase_truncate.as_millis() as u64,
        walk_ms = phase_walk.as_millis() as u64,
        write_total_ms = phase_write_total.as_millis() as u64,
        scan_meta_ms = phase_scan_meta.as_millis() as u64,
        dirs = counters.dirs_visited.load(Ordering::Relaxed),
        files = counters.files_visited.load(Ordering::Relaxed),
        symlinks = counters.symlinks_visited.load(Ordering::Relaxed),
        files_reused = counters.files_reused.load(Ordering::Relaxed),
        files_rehashed = counters.files_rehashed.load(Ordering::Relaxed),
        bytes_hashed = bytes_hashed,
        bytes_saved_by_cache = bytes_saved,
        ignored = counters.ignored_entries.load(Ordering::Relaxed),
        stat_errors = counters.stat_errors.load(Ordering::Relaxed),
        read_errors = counters.read_errors.load(Ordering::Relaxed),
        non_utf8_paths = counters.non_utf8_paths.load(Ordering::Relaxed),
        batches_emitted = counters.batches_emitted.load(Ordering::Relaxed),
        mb_per_s = mb_per_s,
        entries_per_s = entries_per_s,
    );

    Ok(FetchSummary {
        errors: walker_errors.len(),
        ..summary
    })
}

/// Streaming producer-consumer pipeline. Walker runs on a blocking
/// thread, pushes batches over a bounded mpsc channel; an async
/// writer task drains the channel and commits each batch. Both run
/// concurrently — the walker hashes the next batch while the writer
/// commits the previous one.
async fn streaming_pipeline(
    root: PathBuf,
    default_stamp_kind: StampKind,
    prev_stats: std::collections::HashMap<String, FileStatsRow>,
    prev_file_blake3s: std::collections::HashMap<String, hash::Blake3>,
    db: RawDb,
    now: String,
    progress: Progress,
) -> Result<(
    FetchSummary,
    Vec<walker::WalkerError>,
    Arc<WalkerCounters>,
    Duration,
    Duration,
)> {
    let (tx, mut rx) = mpsc::channel::<Batch>(BATCH_CHANNEL_CAPACITY);
    let counters = Arc::new(WalkerCounters::default());
    let stop_progress = Arc::new(AtomicBool::new(false));

    // ── Writer task ──────────────────────────────────────────────────
    // One sqlite transaction PER BATCH. We deliberately do NOT fold the
    // whole scan into a single transaction: doltlite buffers an open
    // transaction's working-set delta in memory, and a single tx over a
    // multi-million-row tree OOMs (confirmed at 4.5M rows × the table
    // set). Per-batch flushing bounds that buffer. The cost is
    // write-amplification (each sqlite COMMIT lays down fresh prolly
    // chunk novelty), reclaimed by the `dolt_gc` the orchestrator runs
    // after the single `dolt_commit`. `BATCH_SIZE` (see walker) is the
    // knob that trades memory against amplification.
    let writer_db = db.clone();
    let writer_now = now.clone();
    let writer_handle = tokio::spawn(async move {
        let mut total_write = Duration::ZERO;
        let mut batches_written: u64 = 0;
        while let Some((files, stats)) = rx.recv().await {
            let took = writer_db.write_batch(&files, &stats, &writer_now).await?;
            total_write += took;
            batches_written += 1;
        }
        Ok::<(Duration, u64), anyhow::Error>((total_write, batches_written))
    });

    // ── Progress task ────────────────────────────────────────────────
    let progress_counters = counters.clone();
    let progress_stop = stop_progress.clone();
    let progress_sink = progress.clone();
    let progress_handle = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_millis(PROGRESS_INTERVAL_MS));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // The total file count is unknown up front, so the bar runs in
        // spinner mode; advancing its position by the files-visited
        // delta each tick gives a live count + a files/sec throughput
        // readout, while the message carries the richer breakdown.
        let mut last_files = 0u64;
        loop {
            interval.tick().await;
            if progress_stop.load(Ordering::Relaxed) {
                break;
            }
            let dirs = progress_counters.dirs_visited.load(Ordering::Relaxed);
            let files = progress_counters.files_visited.load(Ordering::Relaxed);
            let reused = progress_counters.files_reused.load(Ordering::Relaxed);
            let mb_hashed = progress_counters.bytes_hashed.load(Ordering::Relaxed) / 1_000_000;
            let mb_saved = progress_counters
                .bytes_skipped_cache
                .load(Ordering::Relaxed)
                / 1_000_000;
            progress_sink.inc(files.saturating_sub(last_files));
            last_files = files;
            progress_sink.set_message(&format!(
                "scanned {dirs} dirs / {files} files / {reused} cached / {mb_hashed} MB hashed / {mb_saved} MB saved",
            ));
            info!(
                event = "fsindex_progress",
                dirs = dirs,
                files = files,
                files_reused = reused,
                bytes_hashed = progress_counters.bytes_hashed.load(Ordering::Relaxed),
                bytes_saved = progress_counters
                    .bytes_skipped_cache
                    .load(Ordering::Relaxed),
                ignored = progress_counters.ignored_entries.load(Ordering::Relaxed),
                stat_errors = progress_counters.stat_errors.load(Ordering::Relaxed),
                read_errors = progress_counters.read_errors.load(Ordering::Relaxed),
            );
        }
    });

    // ── Walker task on blocking thread ───────────────────────────────
    let walker_counters = counters.clone();
    let walker_root = root.clone();
    let walker_handle = tokio::task::spawn_blocking(
        move || -> Result<(Vec<walker::WalkerError>, walker::WalkerSummary, Duration)> {
            let started = Instant::now();
            let walker = walker::Walker::new(
                &walker_root,
                &prev_stats,
                &prev_file_blake3s,
                default_stamp_kind,
            );
            let (errs, summ) = walker.collect_streaming(&walker_counters, |batch| {
                let mut files = Vec::with_capacity(batch.len());
                let mut stats = Vec::with_capacity(batch.len());
                for r in batch {
                    files.push(r.file_row);
                    stats.push(r.stat_row);
                }
                tx.blocking_send((files, stats))
                    .map_err(|e| anyhow::anyhow!("writer channel closed: {e}"))?;
                Ok(())
            })?;
            Ok((errs, summ, started.elapsed()))
        },
    );

    // Wait for walker FIRST (closes tx by dropping when closure exits).
    let walker_join = walker_handle.await.context("walker task join")?;
    // Then the writer drains the rest and finishes.
    let writer_join = writer_handle.await.context("writer task join")?;

    // Stop progress task.
    stop_progress.store(true, Ordering::Relaxed);
    let _ = progress_handle.await;

    // Error-precedence: when the WRITER dies first it drops `rx`, so the
    // walker's `blocking_send` then fails with a generic "channel
    // closed". That masks the real cause. So if the writer errored,
    // surface the writer's error regardless of what the walker said.
    let (phase_write_total, _batches_written) = match writer_join {
        Ok(v) => v,
        Err(writer_err) => {
            return Err(writer_err.context("doltlite writer task failed"));
        }
    };
    let (walker_errors, walker_summary, phase_walk) = walker_join?;

    let entries_scanned = (walker_summary.rehashed + walker_summary.reused)
        + counters.non_utf8_paths.load(Ordering::Relaxed) as usize;
    let summary = FetchSummary {
        entries_scanned,
        entries_rehashed: walker_summary.rehashed,
        entries_reused: walker_summary.reused,
        stamped_directories: 0,
        errors: 0, // filled in by caller
    };

    Ok((
        summary,
        walker_errors,
        counters,
        phase_walk,
        phase_write_total,
    ))
}

/// Legacy collect-all-then-write path. Used when stamping is enabled
/// because stamping mutates each dir's `identity_uuid` after the walk.
async fn legacy_inmemory(
    opts: &FetchOptions,
    default_stamp_kind: StampKind,
    prev_stats: std::collections::HashMap<String, FileStatsRow>,
    prev_file_blake3s: std::collections::HashMap<String, hash::Blake3>,
    db: &RawDb,
    now: &str,
) -> Result<(
    FetchSummary,
    Vec<walker::WalkerError>,
    Arc<WalkerCounters>,
    Duration,
    Duration,
)> {
    let counters = Arc::new(WalkerCounters::default());
    let walk_start = Instant::now();
    let walker = walker::Walker::new(
        &opts.root,
        &prev_stats,
        &prev_file_blake3s,
        default_stamp_kind,
    );
    let (mut scan_results, walker_errors, walker_summary) = {
        let mut out: Vec<walker::ScanResult> = Vec::new();
        let (errs, summ) = walker.collect_streaming(&counters, |batch| {
            out.extend(batch);
            Ok(())
        })?;
        (out, errs, summ)
    };
    let phase_walk = walk_start.elapsed();

    let stamped = stamp_directories(&opts.root, &mut scan_results).await?;
    if stamped > 0 {
        warn!(
            event = "fsindex_stamping_active",
            stamped = stamped,
            message =
                "stamping is on — set `--no-stamp` or remove `stamp_me_with_uuid: true` to disable",
        );
    }

    let entries_scanned = scan_results.len();
    let files: Vec<_> = scan_results.iter().map(|r| r.file_row.clone()).collect();
    let stats: Vec<_> = scan_results.iter().map(|r| r.stat_row.clone()).collect();
    let write_start = Instant::now();
    db.write_batch(&files, &stats, now).await?;
    let phase_write_total = write_start.elapsed();

    let summary = FetchSummary {
        entries_scanned,
        entries_rehashed: walker_summary.rehashed,
        entries_reused: walker_summary.reused,
        stamped_directories: stamped,
        errors: 0,
    };

    Ok((
        summary,
        walker_errors,
        counters,
        phase_walk,
        phase_write_total,
    ))
}

async fn stamp_directories(
    root: &std::path::Path,
    scan_results: &mut [walker::ScanResult],
) -> Result<usize> {
    let mut count = 0_usize;
    for sr in scan_results.iter_mut() {
        if !matches!(sr.file_row.kind, FileKind::Dir) {
            continue;
        }
        let dir = if sr.file_row.id.is_empty() {
            root.to_path_buf()
        } else {
            root.join(&sr.file_row.id)
        };
        let cascade = build_dir_cascade(root, &dir);
        let eff = cascade.effective();
        if !eff.stamp_me_with_uuid {
            continue;
        }
        let mut yaml = options::load_at(&dir)?.unwrap_or_default();
        if let Some(id) = &yaml.identity {
            sr.file_row.identity_uuid = Some(id.uuid.clone());
            continue;
        }
        let uuid = new_uuid();
        let stamped_at = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        let originally_at = dir
            .strip_prefix(root)
            .ok()
            .map(|p| p.to_string_lossy().into_owned());
        yaml.identity = Some(Identity {
            uuid: uuid.clone(),
            stamped_at,
            stamper_version: 1,
            originally_at,
        });
        options::write_breadcrumb(&dir, &yaml)
            .with_context(|| format!("write breadcrumb {}", dir.display()))?;
        sr.file_row.identity_uuid = Some(uuid.clone());
        info!(
            event = "fsindex_stamped",
            path = %dir.display(),
            uuid = %uuid,
        );
        count += 1;
    }
    Ok(count)
}

/// UUIDv7: time-ordered, so breadcrumb UUIDs sort chronologically.
fn new_uuid() -> String {
    uuid::Uuid::now_v7().to_string()
}

fn build_root_cascade(root: &std::path::Path) -> OptionsCascade {
    let mut c = OptionsCascade::new();
    if let Ok(Some(y)) = options::load_at(root) {
        c.push(root.to_path_buf(), y);
    }
    c
}

fn build_dir_cascade(root: &std::path::Path, dir: &std::path::Path) -> OptionsCascade {
    let mut c = OptionsCascade::new();
    let mut cur = root.to_path_buf();
    if let Ok(Some(y)) = options::load_at(&cur) {
        c.push(cur.clone(), y);
    }
    if let Ok(rel) = dir.strip_prefix(root) {
        for part in rel.iter() {
            cur.push(part);
            if let Ok(Some(y)) = options::load_at(&cur) {
                c.push(cur.clone(), y);
            }
        }
    }
    c
}

// Keep types referenced even if not used at the module surface.
const _: Option<EffectiveOptions> = None;
const _: Option<FsindexYaml> = None;
