//! In-process sync worker: claims a job, runs `datalib-dag` with the
//! job's id as the run id, and records how it ended. Everything the run
//! said on the way — step states, log lines, metrics — the runner writes
//! to `system/runs/runs.sqlite` itself; this file never reads it.

use std::collections::VecDeque;
use std::io::Read;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use app_schema::sync_jobs::{JobKind, JobState, SyncJobRow};
use datalib_core::repo::DynAppRepo;
use serde::Serialize;
use tokio::sync::broadcast;

/// A push update for one job, fanned out to SSE subscribers
/// (`GET /api/sync/stream`) the instant the worker writes it — so the UI
/// reflects a job starting or ending without polling. What the run is
/// doing in between reaches the UI another way: the runner's writes to
/// `system/runs/runs.sqlite` are pushed as `table_changed` root frames naming
/// the datasets they feed (`watch.rs`).
#[derive(Debug, Clone, Serialize)]
pub struct ProgressEvent {
    pub id: String,
    pub kind: String,
    /// Comma-separated source-step ids, mirroring
    /// [`SyncJobRow::source_ids`].
    pub source_ids: Option<String>,
    pub state: JobState,
    /// Whether the job still holds the runner after this event — the
    /// same answer [`SyncJobRow::is_active`] gives for the row, so a
    /// reader never has to work it out from `state`. Every event a
    /// worker sends is definitive: `running` while it runs, and its
    /// terminal state only once the runner has exited.
    pub active: bool,
    pub progress_msg: Option<String>,
}

impl ProgressEvent {
    pub fn new(job: &SyncJobRow, state: JobState, msg: Option<String>) -> Self {
        ProgressEvent {
            id: job.id.clone(),
            kind: job.kind.clone(),
            source_ids: job.source_ids.clone(),
            state,
            active: matches!(state, JobState::Pending | JobState::Running),
            progress_msg: msg,
        }
    }
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
/// After a cancel's first SIGTERM, how long to let the steps checkpoint
/// before the second one tells the runner to give up on them.
const CANCEL_GRACE: Duration = Duration::from_secs(15);
/// After that second SIGTERM, how long the runner gets to kill its steps
/// and go before it is killed itself. It only has to signal them, so
/// this is short.
const CANCEL_KILL_GRACE: Duration = Duration::from_secs(5);

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
        tracing::warn!("worker: ${env}={} is not a file", p.display());
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
        tracing::warn!("worker: $DATALIB_STEP_BIN={} is not a file", p.display());
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
        tracing::warn!(
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
    recover(&repo, &cfg).await;
    match &cfg.dag_bin {
        Some(p) => tracing::info!("worker: ready (dag runner: {})", p.display()),
        None => tracing::warn!(
            "worker: no `datalib-dag` binary found (set $DATALIB_DAG_BIN). \
             UI-triggered syncs will fail until it's available; search still works."
        ),
    }
    loop {
        match repo.claim_next_job().await {
            Ok(Some(job)) => {
                let id = job.id.clone();
                if let Err(e) = run_job(&repo, &cfg, job).await {
                    tracing::error!("worker: job {id} errored: {e:#}");
                    let msg = format!("{e:#}");
                    finish(&repo, &id, JobState::Failed, Some(&msg)).await;
                    // Minimal terminal event so the UI stops showing it as
                    // active; it'll refetch the row for the full error.
                    let _ = cfg.progress_tx.send(ProgressEvent {
                        id,
                        kind: String::new(),
                        source_ids: None,
                        state: JobState::Failed,
                        active: false,
                        progress_msg: Some(msg),
                    });
                }
            }
            Ok(None) => tokio::time::sleep(POLL_IDLE).await,
            Err(e) => {
                tracing::error!("worker: claim failed: {e}");
                tokio::time::sleep(POLL_IDLE).await;
            }
        }
    }
}

fn emit(tx: &ProgressTx, job: &SyncJobRow, state: JobState, msg: Option<&str>) {
    let _ = tx.send(ProgressEvent::new(job, state, msg.map(str::to_string)));
}

/// A write to the job row that must land: one that did not leaves the
/// UI showing a job as running, or uncancellable, until the next boot.
/// The store is a doltlite file this process owns; a failed statement
/// is a passing condition, so try once more, and say so at `error`
/// when even that fails.
async fn must_write<F, Fut>(what: &str, job_id: &str, write: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = Result<(), datalib_core::repo::RepoError>>,
{
    let first = match write().await {
        Ok(()) => return,
        Err(e) => e,
    };
    tracing::error!("worker: job {job_id}: {what} failed ({first}); retrying once");
    if let Err(e) = write().await {
        tracing::error!(
            "worker: job {job_id}: {what} failed again ({e}); the row is wrong until the \
             next boot"
        );
    }
}

/// Finish the run store's sentence when the runner could not.
///
/// A runner that exits closes its own run; one that was SIGKILLed —
/// the last rung of a cancel, or a server that went down with it — ran
/// no code to do it with, and leaves `runs.finished_at_utc` NULL and
/// its steps reading `running` for ever. The Manage screen joins
/// against exactly those rows, so the server closes them on the
/// runner's behalf once it knows the runner is gone.
///
/// A no-op in the ordinary case, and silent in it: only a run that was
/// genuinely left open says anything.
async fn close_the_books(root: &std::path::Path, run_id: &str, why: &str) {
    match datalib_runs::close_abandoned_run(
        root,
        run_id,
        datalib_dag::run_state::RunState::Stopped.as_str(),
        why,
    )
    .await
    {
        Ok(closed) if closed.changed_anything() => tracing::warn!(
            run = %run_id,
            run_was_open = closed.run_was_open,
            steps_closed = closed.steps_closed,
            "the runner left its run open; closing it here"
        ),
        Ok(_) => {}
        Err(e) => tracing::error!(run = %run_id, "could not close the run store's run: {e}"),
    }
}

async fn finish(repo: &DynAppRepo, job_id: &str, state: JobState, msg: Option<&str>) {
    must_write("finish_job", job_id, || repo.finish_job(job_id, state, msg)).await;
}

/// What became of every job the previous server left active, decided
/// from what is actually there — the runner's pid, the run store — and
/// written to the row with the reason. A job still `pending` stays
/// queued; the loop claims it.
///
/// The runner exits with the server (`datalib_parent_watch`), so a
/// `running` job here is normally one whose runner died with the last
/// server. The other answers are kept because they are cheap to tell
/// apart and each is what a person would want to read.
async fn recover(repo: &DynAppRepo, cfg: &WorkerConfig) {
    let jobs = match repo.list_jobs(true, 1_000).await {
        Ok(jobs) => jobs,
        Err(e) => {
            tracing::error!("worker: startup recovery could not list jobs: {e}");
            return;
        }
    };
    for job in jobs
        .iter()
        .filter(|j| j.job_state() != Some(JobState::Pending))
    {
        let (state, why) = what_became_of(&cfg.root, job).await;
        close_the_books(&cfg.root, &job.id, &why).await;
        tracing::warn!(
            "worker: job {} recovered as {}: {why}",
            job.id,
            state.as_str()
        );
        finish(repo, &job.id, state, Some(&why)).await;
        emit(&cfg.progress_tx, job, state, Some(&why));
    }
}

async fn what_became_of(root: &std::path::Path, job: &SyncJobRow) -> (JobState, String) {
    let pid = job.pid.and_then(|p| u32::try_from(p).ok());
    let runner_alive = pid.is_some_and(alive);
    if job.job_state() == Some(JobState::Canceled) {
        if let Some(pid) = pid.filter(|_| runner_alive) {
            terminate(pid);
        }
        return (
            JobState::Canceled,
            "canceled by user; the server restarted before the runner had finished stopping".into(),
        );
    }
    if let Some(pid) = pid.filter(|_| runner_alive) {
        // A runner from before this build, or one whose pipe was lost:
        // nobody can record how it ends, so end it.
        terminate(pid);
        return (
            JobState::Failed,
            format!(
                "interrupted: the server restarted while this job ran; its runner (pid {pid}) \
                 was still going and has been told to stop"
            ),
        );
    }
    let run = datalib_runs::snapshot_of(root, Some(&job.id)).await;
    if run.run_id.as_deref() != Some(job.id.as_str()) {
        return (
            JobState::Failed,
            "interrupted: the server stopped while this job ran, before its runner had \
             recorded a run"
                .into(),
        );
    }
    let failed: Vec<&str> = run
        .steps
        .iter()
        .filter(|s| s.state == datalib_dag::run_state::RunState::Failed.as_str())
        .map(|s| s.step.as_str())
        .collect();
    let unfinished: Vec<&str> = run
        .steps
        .iter()
        .filter(|s| !datalib_runs::is_terminal(&s.state))
        .map(|s| s.step.as_str())
        .collect();
    match (run.finished_at_utc, failed.is_empty()) {
        (Some(at), true) => (
            JobState::Done,
            format!("the server stopped while this job ran; the run finished on its own at {at}"),
        ),
        (Some(at), false) => (
            JobState::Failed,
            format!(
                "the server stopped while this job ran; the run finished on its own at {at} \
                 with failed step(s): {}",
                failed.join(", ")
            ),
        ),
        (None, _) => (
            JobState::Failed,
            format!(
                "interrupted: the server stopped while this job ran, and its runner with it; \
                 step(s) mid-run: {}",
                if unfinished.is_empty() {
                    "none".to_string()
                } else {
                    unfinished.join(", ")
                }
            ),
        ),
    }
}

/// Whether `pid` is a live process — not a zombie, which `kill(0)`
/// would still count.
fn alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        let out = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output();
        match out {
            Ok(out) => {
                let stat = String::from_utf8_lossy(&out.stdout);
                let stat = stat.trim();
                !stat.is_empty() && !stat.starts_with('Z')
            }
            Err(_) => false,
        }
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

fn is_reset(job: &SyncJobRow) -> bool {
    job.kind == JobKind::Reset.as_str()
}

/// What a job's `source_ids` become on `datalib-dag`'s command line.
/// A sync names each seed as its own `--sync` (fringe steps, by id;
/// everything downstream follows normal change propagation) and empty
/// means the whole config; a reset hands the whole list to one
/// `--reset`. Ids with commas aren't supported, so the separator is
/// unambiguous.
fn selection_args(job: &SyncJobRow) -> Vec<String> {
    let Some(ids) = job.source_ids.as_deref().filter(|s| !s.is_empty()) else {
        return Vec::new();
    };
    if is_reset(job) {
        return vec!["--reset".into(), ids.into()];
    }
    ids.split(',')
        .filter(|s| !s.is_empty())
        .flat_map(|id| ["--sync".to_string(), id.to_string()])
        .collect()
}

fn terminate(pid: u32) {
    #[cfg(unix)]
    // Safety: plain kill(2); racing a just-exited pid is benign (ESRCH).
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
}

/// How far a cancel has climbed. Three rungs, not two: `datalib-dag`
/// forwards the first signal to its steps as SIGINT and only kills what
/// ignored it on the **second**, so a SIGKILL straight after the grace
/// leaves a step that did not stop with nobody left to signal it — and
/// SIGKILL at the runner runs no Rust, so nothing cleans up after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CancelStage {
    /// SIGTERM sent; the runner is asking its steps to checkpoint.
    Asked,
    /// Second SIGTERM; the runner kills the steps that would not stop.
    GaveUp,
    /// SIGKILL; the runner itself is out of time.
    Killed,
}

/// The rung a cancel is due next, or `None` while the one it is on
/// still has time. Pure so the ladder's arithmetic is testable without
/// a clock or a child process.
fn next_stage(since_term: Duration, stage: CancelStage) -> Option<CancelStage> {
    match stage {
        CancelStage::Asked if since_term > CANCEL_GRACE => Some(CancelStage::GaveUp),
        CancelStage::GaveUp if since_term > CANCEL_GRACE + CANCEL_KILL_GRACE => {
            Some(CancelStage::Killed)
        }
        _ => None,
    }
}

pub async fn run_job(repo: &DynAppRepo, cfg: &WorkerConfig, job: SyncJobRow) -> anyhow::Result<()> {
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
    command.args(selection_args(&job)).args(["--by", "ui"]);
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
    // Both output pipes are drained, and only their tail is kept: the
    // runner records everything a run says in the store itself. stdin is
    // the parent pipe: nothing is written to it, and its closing — this
    // process exiting however it exits — is what tells the runner to
    // stop (`datalib_parent_watch`), so a run never outlives the server
    // that has to record how it ended.
    command
        .stdin(Stdio::piped())
        .env(datalib_parent_watch::ENV_VAR, "0")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let label = job.source_ids.as_deref().unwrap_or("all sources");
    let verb = if is_reset(&job) {
        "resetting"
    } else {
        "syncing"
    };
    let starting = format!("{verb} {label}…");
    repo.update_job_progress(&job.id, None, Some(&starting))
        .await
        .ok();
    emit(&cfg.progress_tx, &job, JobState::Running, Some(&starting));

    let mut child = command
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn {}: {e}", dag_bin.display()))?;
    let pid = child.id();
    // Without the pid on the row a cancel has nothing to signal.
    must_write("set_job_pid", &job.id, || {
        repo.set_job_pid(&job.id, pid as i64)
    })
    .await;

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

    let mut cancel: Option<(Instant, CancelStage)> = None;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        // Cooperative cancel: the HTTP handler flips state to
        // `canceled`; we walk the rungs of `CancelStage` from there.
        match &mut cancel {
            None => {
                if let Ok(Some(row)) = repo.get_job(&job.id).await {
                    if row.job_state() == Some(JobState::Canceled) {
                        terminate(pid);
                        tracing::info!(
                            job = %job.id, pid,
                            "cancel requested; asking the runner to stop its steps"
                        );
                        cancel = Some((Instant::now(), CancelStage::Asked));
                    }
                }
            }
            Some((at, stage)) => {
                if let Some(next) = next_stage(at.elapsed(), *stage) {
                    match next {
                        CancelStage::GaveUp => {
                            terminate(pid);
                            tracing::warn!(
                                job = %job.id, pid, waited_s = CANCEL_GRACE.as_secs(),
                                "the steps have not stopped; telling the runner to kill them"
                            );
                        }
                        CancelStage::Killed => {
                            let _ = child.kill();
                            tracing::warn!(
                                job = %job.id, pid,
                                "the runner did not exit after being told to give up; killing it"
                            );
                        }
                        CancelStage::Asked => unreachable!("the ladder only climbs"),
                    }
                    *stage = next;
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
    // Before the job's own row, so a reader woken by the job's terminal
    // event does not find the run still open.
    close_the_books(
        &cfg.root,
        &job.id,
        "the runner exited without closing this run; the server closed it",
    )
    .await;
    if cancel.is_some() {
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
    use datalib_core::app_store::AppStore;
    use datalib_runs::{Retention, RunWriter, StepRunRow};

    async fn store(root: &std::path::Path) -> DynAppRepo {
        Arc::new(AppStore::open(root).await.unwrap())
    }

    /// A job the last server claimed and never finished, as the next
    /// boot finds it.
    async fn running_job(repo: &DynAppRepo) -> SyncJobRow {
        repo.enqueue_job(JobKind::All, Some("a/ingest"))
            .await
            .unwrap();
        repo.claim_next_job().await.unwrap().expect("claimed")
    }

    fn step_row(step: &str, state: &str) -> StepRunRow {
        StepRunRow {
            step: step.into(),
            state: state.into(),
            attempt: 1,
            updated_at_utc: "2026-09-17T10:00:00Z".into(),
            ..Default::default()
        }
    }

    /// The cancel ladder climbs one rung at a time and stops at the
    /// top, so each signal is sent once rather than on every poll. The
    /// middle rung is the one that matters: skipping straight from the
    /// grace to SIGKILL is what left a step running with nobody to
    /// signal it, because the runner only kills its steps on a *second*
    /// SIGTERM.
    #[test]
    fn a_cancel_asks_then_tells_the_runner_to_give_up_then_kills_it() {
        use CancelStage::*;
        // Every deadline is read off the constants, so tuning one moves
        // the test with it rather than breaking it.
        let tick = Duration::from_millis(1);
        let gave_up_at = CANCEL_GRACE;
        let killed_at = CANCEL_GRACE + CANCEL_KILL_GRACE;

        assert_eq!(next_stage(Duration::ZERO, Asked), None, "let it work");
        assert_eq!(next_stage(gave_up_at, Asked), None, "the grace is not up");
        assert_eq!(next_stage(gave_up_at + tick, Asked), Some(GaveUp));
        // Still `GaveUp` just after that second SIGTERM: the runner is
        // killing its steps and is not out of time yet.
        assert_eq!(next_stage(gave_up_at + tick, GaveUp), None);
        assert_eq!(next_stage(killed_at, GaveUp), None);
        assert_eq!(next_stage(killed_at + tick, GaveUp), Some(Killed));
        assert_eq!(
            next_stage(killed_at * 100, Killed),
            None,
            "nothing above SIGKILL"
        );
    }

    /// A sync names each seed on its own `--sync`; a reset hands the
    /// list, `+blobs` suffixes and all, to one `--reset`.
    #[test]
    fn a_job_selects_steps_by_sync_or_by_reset() {
        let job = |kind: JobKind, ids: Option<&str>| SyncJobRow {
            id: "j".into(),
            kind: kind.as_str().into(),
            source_ids: ids.map(str::to_string),
            parent_job_id: None,
            state: "pending".into(),
            created_at_utc: "2026-09-22T00:00:00Z".into(),
            started_at_utc: None,
            finished_at_utc: None,
            tz_offset: None,
            error: None,
            pid: None,
            progress_pct: None,
            progress_msg: None,
        };
        assert_eq!(
            selection_args(&job(JobKind::All, Some("a/ingest,b/ingest"))),
            ["--sync", "a/ingest", "--sync", "b/ingest"]
        );
        assert_eq!(
            selection_args(&job(
                JobKind::Reset,
                Some("a/ingest+blobs,a/render_markdown")
            )),
            ["--reset", "a/ingest+blobs,a/render_markdown"]
        );
        assert!(selection_args(&job(JobKind::All, None)).is_empty());
    }

    /// A job the backend died while stopping — `canceled` on request,
    /// never stamped finished by a worker that is gone — would otherwise
    /// hold its steps claimed in the UI until the end of time.
    #[tokio::test]
    async fn recovery_closes_a_cancel_the_worker_never_finished() {
        let td = tempfile::tempdir().unwrap();
        let repo = store(td.path()).await;
        let job = running_job(&repo).await;
        repo.request_cancel_job(&job.id).await.unwrap();
        let mid = repo.get_job(&job.id).await.unwrap().unwrap();
        assert!(mid.is_active() && mid.is_stopping());

        let (state, why) = what_became_of(td.path(), &mid).await;
        assert_eq!(state, JobState::Canceled, "still canceled, not failed");
        assert!(why.starts_with("canceled by user"), "{why}");
    }

    /// The runner died with the server before it recorded a run: the
    /// job says so, rather than "backend restarted".
    #[tokio::test]
    async fn a_job_whose_runner_never_recorded_a_run_is_interrupted() {
        let td = tempfile::tempdir().unwrap();
        let repo = store(td.path()).await;
        let job = running_job(&repo).await;
        let (state, why) = what_became_of(td.path(), &job).await;
        assert_eq!(state, JobState::Failed);
        assert!(
            why.contains("before its runner had recorded a run"),
            "{why}"
        );
    }

    /// The run store knows more than the job row: a run that finished
    /// after the server let go is done, or failed with the steps named;
    /// one still going when the runner died names the steps it was on.
    #[tokio::test]
    async fn the_run_store_says_how_a_recovered_job_ended() {
        let td = tempfile::tempdir().unwrap();
        let repo = store(td.path()).await;
        let keep = Retention::default();

        let job = running_job(&repo).await;
        {
            let w =
                RunWriter::start(td.path(), &job.id, "2026-09-17T10:00:00Z", None, keep).unwrap();
            w.step(step_row("a/ingest", "succeeded"));
        }
        let (state, why) = what_became_of(td.path(), &job).await;
        assert_eq!(state, JobState::Done, "{why}");
        assert!(why.contains("finished on its own"), "{why}");
        repo.finish_job(&job.id, state, Some(&why)).await.unwrap();

        let job = running_job(&repo).await;
        {
            let w =
                RunWriter::start(td.path(), &job.id, "2026-09-17T10:00:00Z", None, keep).unwrap();
            w.step(step_row("a/ingest", "failed"));
            w.step(step_row("a/render_markdown", "blocked"));
        }
        let (state, why) = what_became_of(td.path(), &job).await;
        assert_eq!(state, JobState::Failed);
        assert!(why.contains("failed step(s): a/ingest"), "{why}");
        assert!(!why.contains("render_markdown"), "{why}");
        repo.finish_job(&job.id, state, Some(&why)).await.unwrap();

        let job = running_job(&repo).await;
        // Kept alive across the look, the way a SIGKILLed runner's
        // record is: started, never stamped finished.
        let w = RunWriter::start(td.path(), &job.id, "2026-09-17T10:00:00Z", None, keep).unwrap();
        w.step(step_row("a/ingest", "succeeded"));
        w.step(step_row("a/render_markdown", "running"));
        tokio::time::sleep(Duration::from_millis(600)).await;
        let (state, why) = what_became_of(td.path(), &job).await;
        assert_eq!(state, JobState::Failed);
        assert!(why.contains("step(s) mid-run: a/render_markdown"), "{why}");
        assert!(!why.contains("a/ingest"), "{why}");
        drop(w);
    }

    /// A store whose first `finish_job` fails: the write must land on
    /// the retry. Before this a failed `finish_job` was dropped on the
    /// floor, and the job showed as running until the next boot.
    struct FailsOnce {
        inner: DynAppRepo,
        failed: std::sync::atomic::AtomicBool,
    }

    #[async_trait::async_trait]
    impl datalib_core::repo::AppRepo for FailsOnce {
        async fn get_job(
            &self,
            job_id: &str,
        ) -> Result<Option<SyncJobRow>, datalib_core::repo::RepoError> {
            self.inner.get_job(job_id).await
        }
        async fn finish_job(
            &self,
            job_id: &str,
            state: JobState,
            error: Option<&str>,
        ) -> Result<(), datalib_core::repo::RepoError> {
            if !self.failed.swap(true, std::sync::atomic::Ordering::SeqCst) {
                return Err(datalib_core::repo::RepoError::Internal(
                    "database is locked".into(),
                ));
            }
            self.inner.finish_job(job_id, state, error).await
        }
    }

    #[tokio::test]
    async fn a_failed_finish_is_retried_and_lands() {
        let td = tempfile::tempdir().unwrap();
        let real = store(td.path()).await;
        let job = running_job(&real).await;
        let flaky: DynAppRepo = Arc::new(FailsOnce {
            inner: real.clone(),
            failed: std::sync::atomic::AtomicBool::new(false),
        });
        finish(&flaky, &job.id, JobState::Done, None).await;
        let after = real.get_job(&job.id).await.unwrap().unwrap();
        assert_eq!(after.job_state(), Some(JobState::Done));
        assert!(!after.is_active());
    }

    /// Boot recovery writes every active job's outcome and leaves the
    /// queue's pending jobs for the loop.
    #[tokio::test]
    async fn recovery_finishes_the_active_jobs_and_keeps_the_pending_ones() {
        let td = tempfile::tempdir().unwrap();
        let repo = store(td.path()).await;
        let running = running_job(&repo).await;
        let pending = repo.enqueue_job(JobKind::All, None).await.unwrap();
        let cfg = WorkerConfig {
            root: Arc::new(td.path().to_path_buf()),
            dag_bin: None,
            binary_dir: None,
            progress_tx: broadcast::channel(16).0,
        };
        recover(&repo, &cfg).await;
        let after = repo.get_job(&running.id).await.unwrap().unwrap();
        assert_eq!(after.job_state(), Some(JobState::Failed));
        assert!(after.error.unwrap().starts_with("interrupted"));
        assert!(after.finished_at_utc.is_some());
        let still = repo.get_job(&pending.id).await.unwrap().unwrap();
        assert_eq!(still.job_state(), Some(JobState::Pending));
    }

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
