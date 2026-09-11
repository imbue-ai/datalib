//! In-process sync worker: claims a job, runs `datalib-dag` with the
//! job's id as the run id, and records how it ended. Everything the run
//! said on the way — step states, log lines, metrics — the runner writes
//! to `system/runs.sqlite` itself; this file never reads it.

use std::collections::VecDeque;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use app_schema::sync_jobs::{JobState, SyncJobRow};
use datalib_core::repo::DynAppRepo;
use serde::Serialize;
use tokio::sync::broadcast;

/// A push update for one job, fanned out to SSE subscribers
/// (`GET /api/sync/stream`) the instant the worker writes it — so the UI
/// reflects a job starting or ending without polling. What the run is
/// doing in between reaches the UI another way: the runner's writes to
/// `system/runs.sqlite` are pushed as `dag_changed` root frames.
#[derive(Debug, Clone, Serialize)]
pub struct ProgressEvent {
    pub id: String,
    pub kind: String,
    /// Comma-separated source-step ids, mirroring
    /// [`SyncJobRow::source_ids`].
    pub source_ids: Option<String>,
    pub state: JobState,
    pub progress_msg: Option<String>,
}

/// Broadcast sender shared by the worker and the HTTP enqueue/cancel
/// handlers; the SSE endpoint subscribes to it. A `send` with no
/// subscribers is a no-op (returns `Err`), which we ignore.
pub type ProgressTx = broadcast::Sender<ProgressEvent>;

/// How many of the runner's last lines to keep for the failure message.
/// The run store has everything a step said; this is for what the
/// *runner* said when it could not get as far as a run — a config it
/// refused, a binary it could not find.
const TAIL_LINES: usize = 40;

/// Everything the worker needs that isn't the repo: where the data root
/// is (for the per-job log dir and the config the runner is driven
/// against), and where the binaries live. `dag_bin == None` means we
/// couldn't find the runner — claimed jobs then fail fast with a clear
/// message rather than hanging in `pending` forever.
#[derive(Clone)]
pub struct WorkerConfig {
    pub root: Arc<PathBuf>,
    /// The `datalib-dag` runner binary.
    pub dag_bin: Option<PathBuf>,
    /// Directory holding the step binaries (`datalib-step`, …), passed
    /// via `--binary-dir` so the runner prepends it to every step's
    /// PATH. `None` lets the runner's own fallback chain (config
    /// `binary_dir:`, the runner's own directory) resolve it.
    pub binary_dir: Option<PathBuf>,
    /// Push progress to SSE subscribers as it happens.
    pub progress_tx: ProgressTx,
}

/// Idle queue poll cadence.
const POLL_IDLE: Duration = Duration::from_millis(1000);
/// While a child is running, how often we re-check for a cancel request.
const POLL_RUNNING: Duration = Duration::from_millis(400);
/// After a cancel's SIGTERM, how long to let steps checkpoint before
/// SIGKILL.
const CANCEL_GRACE: Duration = Duration::from_secs(15);

/// Resolve a worker-spawned binary: `$<ENV>` first (how `dev.sh` /
/// `serve_dev.sh` wire it from Bazel runfiles), then a sibling of the
/// running `datalib-http` executable (how a packaged release lays
/// them out side by side).
fn resolve_bin(env: &str, names: &[&str]) -> Option<PathBuf> {
    if let Ok(p) = std::env::var(env) {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
        eprintln!("worker: ${env}={} is not a file", p.display());
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            for name in names {
                let cand = dir.join(name);
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
    }
    None
}

pub fn resolve_dag_bin() -> Option<PathBuf> {
    resolve_bin("DATALIB_DAG_BIN", &["datalib-dag", "datalib_dag_bin"])
}

pub fn resolve_step_bin() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("DATALIB_STEP_BIN") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Some(p);
        }
        eprintln!("worker: $DATALIB_STEP_BIN={} is not a file", p.display());
    }
    if let Some(dir) = resolve_binary_dir() {
        for name in ["datalib-step", "datalib_step"] {
            let cand = dir.join(name);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    resolve_bin("DATALIB_STEP_BIN", &["datalib-step", "datalib_step"])
}

/// Resolve the step-binary directory handed to the runner as
/// `--binary-dir`: `$DATALIB_BINARY_DIR` (how `dev.sh` /
/// `serve_dev.sh` wire a shim dir from Bazel runfiles), else this
/// executable's own directory when `datalib-step` sits next to it
/// (how a packaged release lays the binaries out side by side).
pub fn resolve_binary_dir() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("DATALIB_BINARY_DIR") {
        let p = PathBuf::from(p);
        if p.is_dir() {
            return Some(p);
        }
        eprintln!(
            "worker: $DATALIB_BINARY_DIR={} is not a directory",
            p.display()
        );
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    dir.join("datalib-step")
        .is_file()
        .then(|| dir.to_path_buf())
}

pub async fn run(repo: DynAppRepo, cfg: WorkerConfig) {
    match repo.recover_running_jobs().await {
        Ok(0) => {}
        Ok(n) => eprintln!("worker: recovered {n} orphaned running job(s) → failed"),
        Err(e) => eprintln!("worker: startup recovery failed: {e}"),
    }
    match &cfg.dag_bin {
        Some(p) => eprintln!("worker: ready (dag runner: {})", p.display()),
        None => eprintln!(
            "worker: no `datalib-dag` binary found (set $DATALIB_DAG_BIN). \
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
                    let _ = repo.finish_job(&id, JobState::Failed, Some(&msg)).await;
                    // Minimal terminal event so the UI stops showing it as
                    // active; it'll refetch the row for the full error.
                    let _ = cfg.progress_tx.send(ProgressEvent {
                        id,
                        kind: String::new(),
                        source_ids: None,
                        state: JobState::Failed,
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

fn emit(tx: &ProgressTx, job: &SyncJobRow, state: JobState, msg: Option<&str>) {
    let _ = tx.send(ProgressEvent {
        id: job.id.clone(),
        kind: job.kind.clone(),
        source_ids: job.source_ids.clone(),
        state,
        progress_msg: msg.map(str::to_string),
    });
}

fn terminate(pid: u32) {
    #[cfg(unix)]
    // Safety: plain kill(2); racing a just-exited pid is benign (ESRCH).
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
}

async fn run_job(repo: &DynAppRepo, cfg: &WorkerConfig, job: SyncJobRow) -> anyhow::Result<()> {
    let Some(dag_bin) = cfg.dag_bin.as_ref() else {
        anyhow::bail!("datalib-dag binary not found — set $DATALIB_DAG_BIN to its path");
    };
    // Resolved per job, not once at boot: the Setup tab can create the
    // config while the worker is already running.
    let config_path = datalib_dag::config::root_config_path(&cfg.root);
    if !config_path.is_file() {
        anyhow::bail!(
            "no config at {} — create one from the Setup tab before syncing",
            config_path.display()
        );
    }

    let mut command = Command::new(dag_bin);
    command.arg(&config_path);
    // The job id is the run id: one identity for the queue's row, the
    // runner's record and every row the run store writes.
    command.arg("--run-id").arg(&job.id);
    if let Some(binary_dir) = cfg.binary_dir.as_ref() {
        command.arg("--binary-dir").arg(binary_dir);
    }
    // Per-source "Sync now": subset-sync the selected source steps
    // (fringe steps, by id); everything downstream follows normal
    // change propagation. `source_ids` may carry several
    // comma-separated step ids (the UI's "Sync selected" checkboxes) —
    // ids with commas aren't supported, so the separator is
    // unambiguous. (The old `ingest`/`render` kinds had a
    // `--skip-extract` shortcut; the DAG runner has no equivalent —
    // downloads re-poll and everything unchanged skips, which is the
    // same outcome a little slower.)
    if let Some(srcs) = job.source_ids.as_deref().filter(|s| !s.is_empty()) {
        for src in srcs.split(',').filter(|s| !s.is_empty()) {
            command.arg("--sync").arg(src);
        }
    }
    // Make ~/.datalib/bin resolvable from the config's `command:` lines
    // (step processes inherit the runner's env). Prepended even when
    // the dir doesn't exist yet — an agent may create it between runs,
    // and a missing PATH entry is harmless.
    if let Some(bin) = crate::user_bin_dir() {
        let path = std::env::var_os("PATH").unwrap_or_default();
        let parts = std::iter::once(bin).chain(std::env::split_paths(&path));
        if let Ok(joined) = std::env::join_paths(parts) {
            command.env("PATH", joined);
        }
    }
    // Both pipes are drained, and only their tail is kept: the runner
    // records everything a run says in the store itself.
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let label = job.source_ids.as_deref().unwrap_or("all sources");
    let starting = format!("syncing {label}…");
    repo.update_job_progress(&job.id, None, Some(&starting))
        .await
        .ok();
    emit(&cfg.progress_tx, &job, JobState::Running, Some(&starting));

    let mut child = command
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn {}: {e}", dag_bin.display()))?;
    let pid = child.id();
    repo.set_job_pid(&job.id, pid as i64).await.ok();

    let tail = Arc::new(Mutex::new(VecDeque::with_capacity(TAIL_LINES)));
    let mut readers = Vec::new();
    let mut streams: Vec<Box<dyn Read + Send>> = Vec::new();
    if let Some(o) = child.stdout.take() {
        streams.push(Box::new(o));
    }
    if let Some(e) = child.stderr.take() {
        streams.push(Box::new(e));
    }
    for stream in streams {
        let tail = tail.clone();
        readers.push(std::thread::spawn(move || pump(stream, &tail)));
    }

    let mut term_sent: Option<Instant> = None;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        // Cooperative cancel: the HTTP handler flips state to
        // `canceled`; we send SIGTERM (graceful — steps checkpoint),
        // escalating to SIGKILL after a grace period.
        match term_sent {
            None => {
                if let Ok(Some(row)) = repo.get_job(&job.id).await {
                    if row.job_state() == Some(JobState::Canceled) {
                        terminate(pid);
                        term_sent = Some(Instant::now());
                    }
                }
            }
            Some(t0) => {
                if t0.elapsed() > CANCEL_GRACE {
                    let _ = child.kill();
                }
            }
        }
        tokio::time::sleep(POLL_RUNNING).await;
    };

    // Child has exited; join readers so the tail holds its last words
    // before we record the outcome.
    for h in readers {
        let _ = h.join();
    }
    if term_sent.is_some() {
        repo.finish_job(&job.id, JobState::Canceled, Some("canceled by user"))
            .await?;
        emit(
            &cfg.progress_tx,
            &job,
            JobState::Canceled,
            Some("canceled by user"),
        );
        return Ok(());
    }
    if status.success() {
        repo.update_job_progress(&job.id, Some(1.0), None)
            .await
            .ok();
        repo.finish_job(&job.id, JobState::Done, None).await?;
        emit(&cfg.progress_tx, &job, JobState::Done, None);
    } else {
        // The per-step story is in the run store. What is not is
        // anything the runner said before or instead of a run — a
        // config it refused, a step it could not spawn — and that is
        // what the tail carries.
        let summary = failure_summary(status, &tail.lock().unwrap_or_else(|e| e.into_inner()));
        repo.finish_job(&job.id, JobState::Failed, Some(&summary))
            .await?;
        emit(&cfg.progress_tx, &job, JobState::Failed, Some(&summary));
    }
    Ok(())
}

fn failure_summary(status: std::process::ExitStatus, tail: &VecDeque<String>) -> String {
    // The runner's event lines are already in the store; only what a
    // person could not find there belongs in the message.
    let plain: Vec<&str> = tail
        .iter()
        .map(String::as_str)
        .filter(|l| !l.starts_with('{'))
        .collect();
    if plain.is_empty() {
        format!("datalib-dag exited with {status}")
    } else {
        format!("datalib-dag exited with {status}:\n{}", plain.join("\n"))
    }
}

/// Read a child pipe to EOF, splitting on `\n` *and* `\r` (the latter so
/// `\r`-updated bars from wrapped tools stream too), keeping the last
/// [`TAIL_LINES`] segments.
fn pump(mut rd: Box<dyn Read + Send>, tail: &Mutex<VecDeque<String>>) {
    let mut buf = [0u8; 8192];
    let mut seg: Vec<u8> = Vec::with_capacity(256);
    loop {
        match rd.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                for &b in &buf[..n] {
                    if b == b'\n' || b == b'\r' {
                        push_segment(&seg, tail);
                        seg.clear();
                    } else {
                        seg.push(b);
                    }
                }
            }
            Err(_) => break,
        }
    }
    push_segment(&seg, tail);
}

fn push_segment(seg: &[u8], tail: &Mutex<VecDeque<String>>) {
    if seg.is_empty() {
        return;
    }
    let mut t = tail.lock().unwrap_or_else(|e| e.into_inner());
    if t.len() == TAIL_LINES {
        t.pop_front();
    }
    t.push_back(String::from_utf8_lossy(seg).into_owned());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tail_of(lines: &[&str]) -> VecDeque<String> {
        lines.iter().map(|l| l.to_string()).collect()
    }

    /// The runner's own words survive into the job's error; its event
    /// stream, which the store already holds, does not.
    #[test]
    fn failure_summary_keeps_plain_lines_and_drops_events() {
        use std::os::unix::process::ExitStatusExt;
        let status = std::process::ExitStatus::from_raw(2 << 8);
        let tail = tail_of(&[
            r#"{"event":"step_finish","step":"a","status":"failed"}"#,
            "config.toml: [[steps]] #2: unknown key `foo`",
        ]);
        let s = failure_summary(status, &tail);
        assert!(s.contains("unknown key `foo`"), "{s}");
        assert!(!s.contains("step_finish"), "{s}");

        let s = failure_summary(status, &VecDeque::new());
        assert_eq!(s, "datalib-dag exited with exit status: 2");
    }

    /// The tail is bounded: a chatty run must not turn into an
    /// unbounded buffer in the worker.
    #[test]
    fn pump_keeps_only_the_last_lines() {
        let text: String = (0..(TAIL_LINES * 3))
            .map(|i| format!("line {i}\n"))
            .collect();
        let tail = Mutex::new(VecDeque::new());
        pump(Box::new(std::io::Cursor::new(text.into_bytes())), &tail);
        let t = tail.into_inner().unwrap();
        assert_eq!(t.len(), TAIL_LINES);
        assert_eq!(t.back().map(String::as_str), Some("line 119"));
    }
}
