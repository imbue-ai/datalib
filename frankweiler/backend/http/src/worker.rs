//! In-process sync worker.
//!
//! `frankweiler-http` spawns one instance of [`run`] as a background
//! task at startup. It drains the `sync_jobs` queue the HTTP handlers
//! fill (`POST /api/sync/jobs`): claim the oldest `pending` row, shell
//! out to the `frankweiler-sync` binary against the data root's
//! `config.yaml`, stream the child's stdout+stderr to
//! `<root>/system/state/job-logs/<id>.log` (which `GET /api/sync/jobs/{id}/log`
//! tails live), and write the terminal state back into the queue.
//!
//! Exactly one worker runs per backend process, so claiming needs no
//! cross-worker locking beyond SQLite's single-writer guarantee.
//! Cancellation is cooperative: the cancel handler flips the row to
//! `canceled`; the worker notices on its next poll and kills the child.
//!
//! We use `std::process` (not `tokio::process`) because the workspace
//! tokio build doesn't enable the `process` feature. Spawning and
//! `try_wait()` are non-blocking enough to call straight from the async
//! task; we never block the runtime on `wait()`.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use app_schema::sync_jobs::SyncJobRow;
use frankweiler_core::repo::DynRepo;
use serde::Serialize;
use tokio::sync::broadcast;

/// A push update for one job, fanned out to SSE subscribers
/// (`GET /api/sync/stream`) the instant the worker writes it — so the UI
/// reflects progress without polling. Carries just enough for the client
/// to patch its job list in place; terminal states prompt it to refetch
/// the row for finished-at timestamps.
#[derive(Debug, Clone, Serialize)]
pub struct ProgressEvent {
    pub id: String,
    pub kind: String,
    pub source_name: Option<String>,
    pub state: String,
    pub progress_pct: Option<f64>,
    pub progress_msg: Option<String>,
}

/// Broadcast sender shared by the worker and the HTTP enqueue/cancel
/// handlers; the SSE endpoint subscribes to it. A `send` with no
/// subscribers is a no-op (returns `Err`), which we ignore.
pub type ProgressTx = broadcast::Sender<ProgressEvent>;

/// Ordered, coarse pipeline stages we can recognize from sync's output.
/// We only ever know *which* stage is running (not a true fraction
/// within it), so progress is reported as a discrete "step x/n" rather
/// than a faked continuous percentage. Index into this with
/// [`LiveProgress::stage`].
const STAGES: &[&str] = &["Download", "Ingest", "Index", "Embed"];

/// Latest progress scraped from the child's output, shared between the
/// reader threads (writers) and the async job loop (reader). `stage` is
/// the furthest-along [`STAGES`] index seen so far (monotonic, so it
/// never regresses across interleaved per-source phase lines); `detail`
/// is optional *real* sub-progress for the current stage (e.g. qmd's
/// `59%` embed figure) — never invented.
#[derive(Default)]
struct LiveProgress {
    stage: Option<usize>,
    detail: Option<String>,
}

impl LiveProgress {
    /// Render to the `(progress_pct, progress_msg)` pair stored on the
    /// job row. `progress_pct` is the honest discrete fraction
    /// `stage/total` (filled segments), not an intra-stage guess;
    /// `progress_msg` is the `step x/n: Name` string the UI parses to
    /// drive its segmented bar. `(None, None)` until the first stage is
    /// recognized, so the UI shows an indeterminate bar meanwhile.
    fn render(&self) -> (Option<f64>, Option<String>) {
        match self.stage {
            None => (None, None),
            Some(s) => {
                let n = STAGES.len();
                let pct = (s + 1) as f64 / n as f64;
                let msg = match &self.detail {
                    Some(d) => format!("step {}/{}: {} ({d})", s + 1, n, STAGES[s]),
                    None => format!("step {}/{}: {}", s + 1, n, STAGES[s]),
                };
                (Some(pct), Some(msg))
            }
        }
    }
}

/// Everything the worker needs that isn't the repo: where the data root
/// is (for the per-job log dir), which config to drive sync against, and
/// where the `frankweiler-sync` binary lives. `sync_bin == None` means
/// we couldn't find it — claimed jobs then fail fast with a clear
/// message rather than hanging in `pending` forever.
#[derive(Clone)]
pub struct WorkerConfig {
    pub root: Arc<PathBuf>,
    pub config_path: PathBuf,
    pub sync_bin: Option<PathBuf>,
    /// Push progress to SSE subscribers as it happens.
    pub progress_tx: ProgressTx,
}

/// Idle queue poll cadence.
const POLL_IDLE: Duration = Duration::from_millis(1000);
/// While a child is running, how often we flush the latest scraped
/// progress to the DB and re-check for a cancel request. Kept short so
/// the UI (which polls active jobs) sees progress move in ~real time.
const POLL_RUNNING: Duration = Duration::from_millis(400);

/// Resolve the `frankweiler-sync` binary path: `$FRANKWEILER_SYNC_BIN`
/// first (how `dev.sh` / `serve_dev.sh` wire it from Bazel runfiles),
/// then a sibling of the running `frankweiler-http` executable (how a
/// packaged release lays them out side by side).
pub fn resolve_sync_bin() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("FRANKWEILER_SYNC_BIN") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
        eprintln!(
            "worker: $FRANKWEILER_SYNC_BIN={} is not a file",
            p.display()
        );
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for name in ["frankweiler-sync", "frankweiler_sync_bin"] {
                let cand = dir.join(name);
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
    }
    None
}

/// The worker's main loop. Runs until the process exits.
pub async fn run(repo: DynRepo, cfg: WorkerConfig) {
    match repo.recover_running_jobs().await {
        Ok(0) => {}
        Ok(n) => eprintln!("worker: recovered {n} orphaned running job(s) → failed"),
        Err(e) => eprintln!("worker: startup recovery failed: {e}"),
    }
    match &cfg.sync_bin {
        Some(p) => eprintln!("worker: ready (sync bin: {})", p.display()),
        None => eprintln!(
            "worker: no `frankweiler-sync` binary found (set $FRANKWEILER_SYNC_BIN). \
             UI-triggered syncs will fail until it's available; search still works."
        ),
    }
    loop {
        match repo.claim_next_job().await {
            Ok(Some(job)) => {
                let id = job.id.clone();
                if let Err(e) = run_job(&repo, &cfg, job).await {
                    eprintln!("worker: job {id} errored: {e:#}");
                    let msg = format!("{e:#}");
                    let _ = repo.finish_job(&id, "failed", Some(&msg)).await;
                    // Minimal terminal event so the UI stops showing it as
                    // active; it'll refetch the row for the full error.
                    let _ = cfg.progress_tx.send(ProgressEvent {
                        id,
                        kind: String::new(),
                        source_name: None,
                        state: "failed".to_string(),
                        progress_pct: None,
                        progress_msg: Some(msg),
                    });
                }
            }
            Ok(None) => tokio::time::sleep(POLL_IDLE).await,
            Err(e) => {
                eprintln!("worker: claim failed: {e}");
                tokio::time::sleep(POLL_IDLE).await;
            }
        }
    }
}

/// Fan a single job update out to SSE subscribers. A send with no
/// listeners is a no-op.
fn emit(tx: &ProgressTx, job: &SyncJobRow, state: &str, pct: Option<f64>, msg: Option<&str>) {
    let _ = tx.send(ProgressEvent {
        id: job.id.clone(),
        kind: job.kind.clone(),
        source_name: job.source_name.clone(),
        state: state.to_string(),
        progress_pct: pct,
        progress_msg: msg.map(str::to_string),
    });
}

async fn run_job(repo: &DynRepo, cfg: &WorkerConfig, job: SyncJobRow) -> anyhow::Result<()> {
    let Some(sync_bin) = cfg.sync_bin.as_ref() else {
        anyhow::bail!("frankweiler-sync binary not found — set $FRANKWEILER_SYNC_BIN to its path");
    };
    if !cfg.config_path.is_file() {
        anyhow::bail!(
            "no config at {} — create one from the Setup tab before syncing",
            cfg.config_path.display()
        );
    }

    // Per-job log file the UI tails via /api/sync/jobs/{id}/log.
    let log_dir = frankweiler_core::layout::state_dir(&cfg.root).join("job-logs");
    std::fs::create_dir_all(&log_dir)?;
    let log_path = log_dir.join(format!("{}.log", job.id));
    let log_file = File::create(&log_path)?;

    let mut command = Command::new(sync_bin);
    command.arg("--config").arg(&cfg.config_path);
    // Map the queue's coarse `kind` onto sync's phase flags. `download`
    // and `all` run the full extract → translate → load → index
    // pipeline; `ingest` / `render` skip the network extract and
    // re-process whatever raw data is already on disk.
    if matches!(job.kind.as_str(), "ingest" | "render") {
        command.arg("--skip-extract");
    }
    // Per-source "Sync now": narrow the run to one source via the env
    // var `Config::enabled_sources` honors. Absent for `kind = all`.
    if let Some(src) = job.source_name.as_deref() {
        command.env("FRANKWEILER_ONLY_SOURCE", src);
    }
    // Pipe stdout+stderr so reader threads can both tee them to the log
    // file AND scrape progress out of them. (We can't redirect straight
    // to the file anymore — that gives a live log but no live progress.)
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let label = job.source_name.as_deref().unwrap_or("all sources");
    let starting = format!("syncing {label}…");
    repo.update_job_progress(&job.id, None, Some(&starting))
        .await
        .ok();
    emit(&cfg.progress_tx, &job, "running", None, Some(&starting));

    let mut child = command
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn {}: {e}", sync_bin.display()))?;
    let pid = child.id();
    repo.set_job_pid(&job.id, pid as i64).await.ok();

    // Drain both pipes on dedicated threads: each segment (split on \n or
    // \r — the latter so qmd's `\r`-updated embed bar streams too) is
    // appended to the log file and parsed for a progress message + a
    // coarse phase fraction. The async loop below samples that shared
    // state and writes it to the DB.
    let progress = Arc::new(Mutex::new(LiveProgress::default()));
    let log = Arc::new(Mutex::new(log_file));
    let mut readers = Vec::new();
    let mut streams: Vec<Box<dyn Read + Send>> = Vec::new();
    if let Some(o) = child.stdout.take() {
        streams.push(Box::new(o));
    }
    if let Some(e) = child.stderr.take() {
        streams.push(Box::new(e));
    }
    for stream in streams {
        let log = log.clone();
        let progress = progress.clone();
        readers.push(std::thread::spawn(move || pump(stream, &log, &progress)));
    }

    // Seed `last` with the "syncing…" line we just published so the loop
    // doesn't immediately overwrite it with the empty initial render
    // (no stage recognized yet → `(None, None)`). We only push once a
    // real stage shows up.
    let mut last: Option<(Option<f64>, Option<String>)> = Some((None, Some(starting.clone())));
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        // Flush the latest scraped progress to the DB if it changed —
        // but never regress a known message back to the empty render.
        let snap = {
            let p = progress.lock().unwrap_or_else(|e| e.into_inner());
            p.render()
        };
        if snap == (None, None) {
            tokio::time::sleep(POLL_RUNNING).await;
            continue;
        }
        if last.as_ref() != Some(&snap) {
            repo.update_job_progress(&job.id, snap.0, snap.1.as_deref())
                .await
                .ok();
            emit(&cfg.progress_tx, &job, "running", snap.0, snap.1.as_deref());
            last = Some(snap);
        }
        // Cooperative cancel: the HTTP handler flips state to `canceled`;
        // we observe it here and SIGKILL the child (downloaders are
        // incremental, so a hard kill is safe to resume from).
        if let Ok(Some(row)) = repo.get_job(&job.id).await {
            if row.state == "canceled" {
                let _ = child.kill();
                let _ = child.wait();
                for h in readers {
                    let _ = h.join();
                }
                repo.finish_job(&job.id, "canceled", Some("canceled by user"))
                    .await?;
                emit(
                    &cfg.progress_tx,
                    &job,
                    "canceled",
                    None,
                    Some("canceled by user"),
                );
                return Ok(());
            }
        }
        tokio::time::sleep(POLL_RUNNING).await;
    };

    // Child has exited; join readers so the log file + final progress
    // message reflect every last line before we record the outcome.
    for h in readers {
        let _ = h.join();
    }
    if status.success() {
        repo.update_job_progress(&job.id, Some(1.0), Some("done"))
            .await
            .ok();
        repo.finish_job(&job.id, "done", None).await?;
        emit(&cfg.progress_tx, &job, "done", Some(1.0), Some("done"));
    } else {
        let tail = tail_of(&log_path, 600);
        let summary = if tail.is_empty() {
            format!("frankweiler-sync exited with {status}")
        } else {
            format!("frankweiler-sync exited with {status}: …{tail}")
        };
        repo.finish_job(&job.id, "failed", Some(&summary)).await?;
        emit(&cfg.progress_tx, &job, "failed", None, Some(&summary));
    }
    Ok(())
}

/// Read a child pipe to EOF, splitting on `\n` *and* `\r`. Every segment
/// is appended to the shared log file (so the UI's live tail keeps
/// working) and classified into a pipeline stage to update the shared
/// [`LiveProgress`] (whose stage only ever advances).
fn pump(mut rd: Box<dyn Read + Send>, log: &Mutex<File>, progress: &Mutex<LiveProgress>) {
    let mut buf = [0u8; 8192];
    let mut seg: Vec<u8> = Vec::with_capacity(256);
    loop {
        match rd.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                for &b in &buf[..n] {
                    if b == b'\n' || b == b'\r' {
                        flush_segment(&seg, log, progress);
                        seg.clear();
                    } else {
                        seg.push(b);
                    }
                }
            }
            Err(_) => break,
        }
    }
    flush_segment(&seg, log, progress);
}

fn flush_segment(seg: &[u8], log: &Mutex<File>, progress: &Mutex<LiveProgress>) {
    if seg.is_empty() {
        return;
    }
    let text = String::from_utf8_lossy(seg);
    if let Ok(mut f) = log.lock() {
        let _ = writeln!(f, "{text}");
    }
    // Advance the stage (monotonic) and/or refresh the current stage's
    // real sub-detail. Both are derived purely from recognizable
    // keywords; an unrecognized line leaves the state untouched.
    let stage = stage_of(&text);
    let detail = embed_detail(&text);
    if stage.is_some() || detail.is_some() {
        let mut p = progress.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(idx) = stage {
            let advanced = p.stage.is_none_or(|cur| idx > cur);
            p.stage = Some(p.stage.map_or(idx, |cur| cur.max(idx)));
            // Entering a new stage clears stale detail from the old one.
            if advanced && detail.is_none() {
                p.detail = None;
            }
        }
        if let Some(d) = detail {
            p.detail = Some(d);
        }
    }
}

/// Classify one line of `frankweiler-sync` output into a [`STAGES`] index,
/// or `None` if it doesn't name a recognizable stage. Checked
/// most-advanced-first. Note the `embedd` (double-d) test: it matches
/// qmd's "Embedding/Embedded" activity lines but NOT the literal `qmd
/// embed` advice qmd prints during the *index* stage.
fn stage_of(line: &str) -> Option<usize> {
    let s = line.to_ascii_lowercase();
    if s.contains("embedd") || s.contains("kb/s") || s.contains("eta ") {
        Some(3) // Embed
    } else if s.contains("index") || s.contains("qmd") || s.contains("collection") {
        Some(2) // Index
    } else if s.contains("translate")
        || s.contains("alignment")
        || s.contains("render")
        // NB: guard against "down*load*" — a Download line must not be
        // swallowed by the Ingest "load" token (this branch runs first).
        || (s.contains("load") && !s.contains("download"))
        || s.contains("synth")
    {
        Some(1) // Ingest
    } else if s.contains("extract") || s.contains("download") {
        Some(0) // Download
    } else {
        None
    }
}

/// Pull qmd's *real* embed percentage out of its progress bar, e.g.
/// `██░░ 59% 4540/4587 14.9 KB/s ETA 4m 3s` → `"59%"`. Returned as the
/// stage's `detail` so the segmented bar can annotate the long embed
/// step without inventing a fraction. Gated on the bar's distinctive
/// `ETA` / `KB/s` markers so a stray `%` elsewhere isn't grabbed.
fn embed_detail(s: &str) -> Option<String> {
    if !s.contains('%') || !(s.contains("ETA") || s.contains("KB/s")) {
        return None;
    }
    let bytes = s.as_bytes();
    let pct_pos = s.find('%')?;
    let mut start = pct_pos;
    while start > 0 && bytes[start - 1].is_ascii_digit() {
        start -= 1;
    }
    if start == pct_pos {
        return None;
    }
    Some(format!("{}%", &s[start..pct_pos]))
}

/// Last `max` bytes of a log file, flattened to one line, for the
/// `error` summary column. The full log stays on disk for the UI.
fn tail_of(path: &Path, max: usize) -> String {
    let Ok(s) = std::fs::read_to_string(path) else {
        return String::new();
    };
    let s = s.trim_end();
    let mut start = s.len().saturating_sub(max);
    while start < s.len() && !s.is_char_boundary(start) {
        start += 1;
    }
    s[start..].replace('\n', " ⏎ ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stage_classification() {
        // Download/extract is stage 0 — and must not be swallowed by the
        // Ingest "load" token (down*load*).
        assert_eq!(stage_of("extract: starting slack"), Some(0));
        assert_eq!(stage_of("Downloading channel history"), Some(0));
        assert_eq!(stage_of("download: 14/200"), Some(0));
        // Ingest family.
        assert_eq!(stage_of("translate: rendering docs"), Some(1));
        assert_eq!(stage_of("perseus alignment: 0 sections"), Some(1));
        assert_eq!(stage_of("loading into doltlite"), Some(1));
        // Index then embed (most-advanced-first).
        assert_eq!(stage_of("qmd: updating collection"), Some(2));
        assert_eq!(stage_of("Embedded 10 chunks"), Some(3));
        assert_eq!(stage_of("59% 4540/4587 14.9 KB/s ETA 4m"), Some(3));
        // Unrecognized.
        assert_eq!(stage_of("hello world"), None);
    }

    #[test]
    fn embed_detail_extracts_pct() {
        assert_eq!(
            embed_detail("██░░ 59% 4540/4587 14.9 KB/s ETA 4m 3s").as_deref(),
            Some("59%")
        );
        // No bar markers → not a real figure, so no detail.
        assert_eq!(embed_detail("done 100% of nothing"), None);
        assert_eq!(embed_detail("plain line"), None);
    }

    #[test]
    fn render_is_discrete_and_honest() {
        // No stage yet → indeterminate (no fabricated number).
        assert_eq!(LiveProgress::default().render(), (None, None));
        // Stage 1 (Ingest, 0-indexed) → step 2/4, pct = 2/4.
        let p = LiveProgress {
            stage: Some(1),
            detail: None,
        };
        let (pct, msg) = p.render();
        assert_eq!(pct, Some(0.5));
        assert_eq!(msg.as_deref(), Some("step 2/4: Ingest"));
        // Embed with real sub-detail surfaces it verbatim.
        let p = LiveProgress {
            stage: Some(3),
            detail: Some("59%".into()),
        };
        let (pct, msg) = p.render();
        assert_eq!(pct, Some(1.0));
        assert_eq!(msg.as_deref(), Some("step 4/4: Embed (59%)"));
    }
}
