//! In-process sync worker.
//!
//! `frankweiler-http` spawns one instance of [`run`] as a background
//! task at startup. It drains the `sync_jobs` queue the HTTP handlers
//! fill (`POST /api/sync/jobs`): claim the oldest `pending` row, shell
//! out to the `datalib-dag` runner against the data root's
//! `config.yaml` (the new-format DAG config), stream the child's
//! stdout+stderr to `<root>/system/state/job-logs/<id>.log` (which
//! `GET /api/sync/jobs/{id}/log` tails live), and write the terminal
//! state back into the queue.
//!
//! Progress: the runner emits NDJSON events on stderr (`run_plan`,
//! `step_start`, `step_finish`, `progress_*`, `run_summary`). The
//! worker parses them into a per-task board — there are no pipeline
//! "stages" anymore, only tasks in todo/running/terminal states — and
//! publishes it as JSON in `progress_msg` plus a typed `tasks` list on
//! the SSE event. Multiple tasks run concurrently; all of them carry
//! their own sub-progress.
//!
//! Cancellation is cooperative *and* graceful: the cancel handler
//! flips the row to `canceled`; the worker notices on its next poll
//! and sends the runner SIGTERM, which it forwards to the running
//! step subprocesses as SIGINT so they checkpoint-commit before
//! exiting. SIGKILL only after a grace period.
//!
//! We use `std::process` (not `tokio::process`) because the workspace
//! tokio build doesn't enable the `process` feature. Spawning and
//! `try_wait()` are non-blocking enough to call straight from the async
//! task; we never block the runtime on `wait()`.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
    /// Per-task board, in plan order. `None` until the runner has
    /// announced its plan.
    pub tasks: Option<Vec<TaskState>>,
}

/// One DAG task's state as shown to the UI. `state` is one of
/// `todo` / `running` / `done` / `skipped` / `failed` / `blocked`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TaskState {
    pub id: String,
    pub state: String,
    /// Live sub-progress for running tasks ("123/456 fetching …");
    /// empty otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Broadcast sender shared by the worker and the HTTP enqueue/cancel
/// handlers; the SSE endpoint subscribes to it. A `send` with no
/// subscribers is a no-op (returns `Err`), which we ignore.
pub type ProgressTx = broadcast::Sender<ProgressEvent>;

/// The live task board, built from the runner's NDJSON events. Shared
/// between the pipe-reader threads (writers) and the async job loop
/// (reader).
#[derive(Default)]
struct TaskBoard {
    /// Plan order (from `run_plan`); tasks discovered later (defensive)
    /// append.
    order: Vec<String>,
    tasks: HashMap<String, TaskEntry>,
}

#[derive(Default)]
struct TaskEntry {
    /// `todo` / `running` / `done` / `skipped` / `failed` / `blocked`.
    state: String,
    total: Option<u64>,
    pos: u64,
    msg: Option<String>,
}

impl TaskBoard {
    fn entry(&mut self, id: &str) -> &mut TaskEntry {
        if !self.tasks.contains_key(id) {
            self.order.push(id.to_string());
            self.tasks.insert(
                id.to_string(),
                TaskEntry {
                    state: "todo".into(),
                    ..Default::default()
                },
            );
        }
        self.tasks.get_mut(id).unwrap()
    }

    /// Feed one output line; NDJSON runner events update the board,
    /// everything else is ignored (it's still teed to the log).
    fn apply_line(&mut self, line: &str) {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            return;
        };
        let Some(event) = v.get("event").and_then(|e| e.as_str()) else {
            return;
        };
        let step = v.get("step").and_then(|s| s.as_str());
        match (event, step) {
            ("run_plan", _) => {
                if let Some(steps) = v.get("steps").and_then(|s| s.as_array()) {
                    for s in steps {
                        if let Some(id) = s.as_str() {
                            self.entry(id);
                        }
                    }
                }
            }
            ("step_start", Some(id)) => {
                let e = self.entry(id);
                e.state = "running".into();
            }
            ("step_finish", Some(id)) => {
                let status = v.get("status").and_then(|s| s.as_str()).unwrap_or("");
                let e = self.entry(id);
                e.state = match status {
                    "succeeded" => "done",
                    "skipped_up_to_date" => "skipped",
                    "blocked" => "blocked",
                    _ => "failed",
                }
                .into();
                e.msg = None;
            }
            ("progress_length", Some(id)) => {
                let e = self.entry(id);
                e.total = v.get("total").and_then(|t| t.as_u64());
                e.pos = 0;
            }
            ("progress_inc", Some(id)) => {
                let delta = v.get("delta").and_then(|d| d.as_u64()).unwrap_or(0);
                let e = self.entry(id);
                e.pos += delta;
            }
            ("progress_message", Some(id)) => {
                let msg = v.get("msg").and_then(|m| m.as_str()).unwrap_or("");
                let e = self.entry(id);
                e.msg = Some(msg.to_string());
            }
            ("run_summary", _) => {
                // Authoritative final states (covers anything the
                // per-step events missed, e.g. after a mid-run kill).
                if let Some(steps) = v.get("steps").and_then(|s| s.as_array()) {
                    for s in steps {
                        let (Some(id), Some(status)) = (
                            s.get("step").and_then(|x| x.as_str()),
                            s.get("status").and_then(|x| x.as_str()),
                        ) else {
                            continue;
                        };
                        let e = self.entry(id);
                        e.state = match status {
                            "succeeded" => "done",
                            "skipped_up_to_date" => "skipped",
                            "blocked" => "blocked",
                            _ => "failed",
                        }
                        .into();
                    }
                }
            }
            _ => {}
        }
    }

    fn snapshot(&self) -> Vec<TaskState> {
        self.order
            .iter()
            .filter_map(|id| {
                let e = self.tasks.get(id)?;
                let detail = if e.state == "running" {
                    let counts = e.total.map(|t| format!("{}/{}", e.pos, t));
                    match (counts, &e.msg) {
                        (Some(c), Some(m)) => Some(format!("{c} {m}")),
                        (Some(c), None) => Some(c),
                        (None, Some(m)) => Some(m.clone()),
                        (None, None) => None,
                    }
                } else {
                    None
                };
                Some(TaskState {
                    id: id.clone(),
                    state: e.state.clone(),
                    detail,
                })
            })
            .collect()
    }

    /// Render to the `(progress_pct, progress_msg)` pair stored on the
    /// job row. `progress_pct` is the terminal-task fraction;
    /// `progress_msg` is the task board as JSON (`{"v":1,"tasks":…}`)
    /// so the row alone can rebuild the cell bar after a refetch.
    /// `(None, None, [])` until the plan is known, so the UI shows an
    /// indeterminate bar meanwhile.
    fn render(&self) -> (Option<f64>, Option<String>, Vec<TaskState>) {
        let tasks = self.snapshot();
        if tasks.is_empty() {
            return (None, None, tasks);
        }
        let terminal = tasks
            .iter()
            .filter(|t| matches!(t.state.as_str(), "done" | "skipped" | "failed" | "blocked"))
            .count();
        let pct = terminal as f64 / tasks.len() as f64;
        let msg = serde_json::json!({"v": 1, "tasks": tasks}).to_string();
        (Some(pct), Some(msg), tasks)
    }

    /// Ids of failed tasks (for the terminal summary line).
    fn failed_ids(&self) -> Vec<String> {
        self.order
            .iter()
            .filter(|id| self.tasks.get(*id).is_some_and(|e| e.state == "failed"))
            .cloned()
            .collect()
    }
}

/// Everything the worker needs that isn't the repo: where the data root
/// is (for the per-job log dir), which config to drive the runner
/// against, and where the binaries live. `dag_bin == None` means we
/// couldn't find the runner — claimed jobs then fail fast with a clear
/// message rather than hanging in `pending` forever.
#[derive(Clone)]
pub struct WorkerConfig {
    pub root: Arc<PathBuf>,
    pub config_path: PathBuf,
    /// The `datalib-dag` runner binary.
    pub dag_bin: Option<PathBuf>,
    /// The `datalib-step` binary, passed via `--step-bin`. `None` lets
    /// the runner's own fallback chain (config `step_bin:`, sibling of
    /// the runner, PATH) resolve it.
    pub step_bin: Option<PathBuf>,
    /// Push progress to SSE subscribers as it happens.
    pub progress_tx: ProgressTx,
}

/// Idle queue poll cadence.
const POLL_IDLE: Duration = Duration::from_millis(1000);
/// While a child is running, how often we flush the latest progress to
/// the DB and re-check for a cancel request. Kept short so the UI (which
/// polls active jobs) sees progress move in ~real time.
const POLL_RUNNING: Duration = Duration::from_millis(400);
/// After a cancel's SIGTERM, how long to let steps checkpoint before
/// SIGKILL.
const CANCEL_GRACE: Duration = Duration::from_secs(15);

/// Resolve a worker-spawned binary: `$<ENV>` first (how `dev.sh` /
/// `serve_dev.sh` wire it from Bazel runfiles), then a sibling of the
/// running `frankweiler-http` executable (how a packaged release lays
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

/// Resolve the `datalib-dag` runner: `$FRANKWEILER_DAG_BIN` or a
/// sibling binary.
pub fn resolve_dag_bin() -> Option<PathBuf> {
    resolve_bin("FRANKWEILER_DAG_BIN", &["datalib-dag", "datalib_dag"])
}

/// Resolve the `datalib-step` binary: `$FRANKWEILER_STEP_BIN` or a
/// sibling binary.
pub fn resolve_step_bin() -> Option<PathBuf> {
    resolve_bin("FRANKWEILER_STEP_BIN", &["datalib-step", "datalib_step"])
}

/// The worker's main loop. Runs until the process exits.
pub async fn run(repo: DynRepo, cfg: WorkerConfig) {
    match repo.recover_running_jobs().await {
        Ok(0) => {}
        Ok(n) => eprintln!("worker: recovered {n} orphaned running job(s) → failed"),
        Err(e) => eprintln!("worker: startup recovery failed: {e}"),
    }
    match &cfg.dag_bin {
        Some(p) => eprintln!("worker: ready (dag runner: {})", p.display()),
        None => eprintln!(
            "worker: no `datalib-dag` binary found (set $FRANKWEILER_DAG_BIN). \
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
                        tasks: None,
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
fn emit(
    tx: &ProgressTx,
    job: &SyncJobRow,
    state: &str,
    pct: Option<f64>,
    msg: Option<&str>,
    tasks: Option<Vec<TaskState>>,
) {
    let _ = tx.send(ProgressEvent {
        id: job.id.clone(),
        kind: job.kind.clone(),
        source_name: job.source_name.clone(),
        state: state.to_string(),
        progress_pct: pct,
        progress_msg: msg.map(str::to_string),
        tasks,
    });
}

/// Best-effort SIGTERM (Unix). The runner forwards it to running steps
/// as SIGINT so they checkpoint before exiting.
fn terminate(pid: u32) {
    #[cfg(unix)]
    // Safety: plain kill(2); racing a just-exited pid is benign (ESRCH).
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
}

async fn run_job(repo: &DynRepo, cfg: &WorkerConfig, job: SyncJobRow) -> anyhow::Result<()> {
    let Some(dag_bin) = cfg.dag_bin.as_ref() else {
        anyhow::bail!("datalib-dag binary not found — set $FRANKWEILER_DAG_BIN to its path");
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

    let mut command = Command::new(dag_bin);
    command.arg(&cfg.config_path);
    if let Some(step_bin) = cfg.step_bin.as_ref() {
        command.arg("--step-bin").arg(step_bin);
    }
    // Per-source "Sync now": subset-sync just that source's download
    // step; everything downstream follows normal change propagation.
    // Relies on the `<name>.download` id convention the config
    // templates use. (The old `ingest`/`render` kinds had a
    // `--skip-extract` shortcut; the DAG runner has no equivalent —
    // downloads re-poll and everything unchanged skips, which is the
    // same outcome a little slower.)
    if let Some(src) = job.source_name.as_deref() {
        command.arg("--sync").arg(format!("{src}.download"));
    }
    // Pipe stdout+stderr so reader threads can both tee them to the log
    // file AND parse the runner's NDJSON events for live progress.
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let label = job.source_name.as_deref().unwrap_or("all sources");
    let starting = format!("syncing {label}…");
    repo.update_job_progress(&job.id, None, Some(&starting))
        .await
        .ok();
    emit(
        &cfg.progress_tx,
        &job,
        "running",
        None,
        Some(&starting),
        None,
    );

    let mut child = command
        .spawn()
        .map_err(|e| anyhow::anyhow!("spawn {}: {e}", dag_bin.display()))?;
    let pid = child.id();
    repo.set_job_pid(&job.id, pid as i64).await.ok();

    // Drain both pipes on dedicated threads: each line is appended to
    // the log file and fed to the task board (runner events arrive on
    // stderr; stdout is the final report — both are teed).
    let board = Arc::new(Mutex::new(TaskBoard::default()));
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
        let board = board.clone();
        readers.push(std::thread::spawn(move || pump(stream, &log, &board)));
    }

    let mut last: Option<Option<String>> = None;
    let mut term_sent: Option<Instant> = None;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        // Flush the latest board to the DB + SSE if it changed.
        let (pct, msg, tasks) = {
            let b = board.lock().unwrap_or_else(|e| e.into_inner());
            b.render()
        };
        if msg.is_some() && last.as_ref() != Some(&msg) {
            repo.update_job_progress(&job.id, pct, msg.as_deref())
                .await
                .ok();
            emit(
                &cfg.progress_tx,
                &job,
                "running",
                pct,
                msg.as_deref(),
                Some(tasks),
            );
            last = Some(msg);
        }
        // Cooperative cancel: the HTTP handler flips state to
        // `canceled`; we send SIGTERM (graceful — steps checkpoint),
        // escalating to SIGKILL after a grace period.
        match term_sent {
            None => {
                if let Ok(Some(row)) = repo.get_job(&job.id).await {
                    if row.state == "canceled" {
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

    // Child has exited; join readers so the log file + final board
    // reflect every last line before we record the outcome.
    for h in readers {
        let _ = h.join();
    }
    let (pct, msg, tasks) = {
        let b = board.lock().unwrap_or_else(|e| e.into_inner());
        b.render()
    };
    if term_sent.is_some() {
        repo.update_job_progress(&job.id, pct, msg.as_deref())
            .await
            .ok();
        repo.finish_job(&job.id, "canceled", Some("canceled by user"))
            .await?;
        emit(
            &cfg.progress_tx,
            &job,
            "canceled",
            pct,
            Some("canceled by user"),
            Some(tasks),
        );
        return Ok(());
    }
    if status.success() {
        repo.update_job_progress(&job.id, Some(1.0), msg.as_deref())
            .await
            .ok();
        repo.finish_job(&job.id, "done", None).await?;
        emit(
            &cfg.progress_tx,
            &job,
            "done",
            Some(1.0),
            msg.as_deref(),
            Some(tasks),
        );
    } else {
        // Blame the failed tasks by name; the full log is one click
        // away.
        let failed = {
            let b = board.lock().unwrap_or_else(|e| e.into_inner());
            b.failed_ids()
        };
        let summary = if failed.is_empty() {
            format!("datalib-dag exited with {status}")
        } else {
            format!(
                "datalib-dag exited with {status} (failed: {})",
                failed.join(", ")
            )
        };
        repo.update_job_progress(&job.id, pct, msg.as_deref())
            .await
            .ok();
        repo.finish_job(&job.id, "failed", Some(&summary)).await?;
        emit(
            &cfg.progress_tx,
            &job,
            "failed",
            pct,
            Some(&summary),
            Some(tasks),
        );
    }
    Ok(())
}

/// Read a child pipe to EOF, splitting on `\n` *and* `\r` (the latter so
/// `\r`-updated bars from wrapped tools stream too). Every segment is
/// appended to the shared log file (so the UI's live tail keeps working)
/// and fed to the shared [`TaskBoard`].
fn pump(mut rd: Box<dyn Read + Send>, log: &Mutex<File>, board: &Mutex<TaskBoard>) {
    let mut buf = [0u8; 8192];
    let mut seg: Vec<u8> = Vec::with_capacity(256);
    loop {
        match rd.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                for &b in &buf[..n] {
                    if b == b'\n' || b == b'\r' {
                        flush_segment(&seg, log, board);
                        seg.clear();
                    } else {
                        seg.push(b);
                    }
                }
            }
            Err(_) => break,
        }
    }
    flush_segment(&seg, log, board);
}

fn flush_segment(seg: &[u8], log: &Mutex<File>, board: &Mutex<TaskBoard>) {
    if seg.is_empty() {
        return;
    }
    let text = String::from_utf8_lossy(seg);
    if let Ok(mut f) = log.lock() {
        let _ = writeln!(f, "{text}");
    }
    let mut b = board.lock().unwrap_or_else(|e| e.into_inner());
    b.apply_line(&text);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feed(board: &mut TaskBoard, lines: &[&str]) {
        for l in lines {
            board.apply_line(l);
        }
    }

    #[test]
    fn board_follows_runner_events() {
        let mut b = TaskBoard::default();
        feed(
            &mut b,
            &[
                r#"{"event":"run_plan","steps":["slack.download","slack.render","index"]}"#,
                r#"{"event":"step_start","step":"slack.download","attempt":1}"#,
                r#"{"event":"progress_length","step":"slack.download","total":10}"#,
                r#"{"event":"progress_inc","step":"slack.download","delta":3}"#,
                r#"{"event":"progress_message","step":"slack.download","msg":"conversations.list"}"#,
                "not json at all",
                r#"{"timestamp":"t","level":"INFO","fields":{}}"#,
            ],
        );
        let (pct, msg, tasks) = b.render();
        assert_eq!(pct, Some(0.0));
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0].state, "running");
        assert_eq!(tasks[0].detail.as_deref(), Some("3/10 conversations.list"));
        assert_eq!(tasks[1].state, "todo");
        // The stored msg is self-contained JSON the UI can rebuild from.
        let v: serde_json::Value = serde_json::from_str(&msg.unwrap()).unwrap();
        assert_eq!(v["v"], 1);
        assert_eq!(v["tasks"][0]["id"], "slack.download");

        feed(
            &mut b,
            &[
                r#"{"event":"step_finish","step":"slack.download","status":"succeeded"}"#,
                r#"{"event":"step_start","step":"slack.render","attempt":1}"#,
                r#"{"event":"step_finish","step":"slack.render","status":"failed","error":"boom"}"#,
                r#"{"event":"step_finish","step":"index","status":"blocked"}"#,
            ],
        );
        let (pct, _, tasks) = b.render();
        assert_eq!(pct, Some(1.0));
        assert_eq!(tasks[0].state, "done");
        assert_eq!(tasks[0].detail, None, "terminal tasks carry no detail");
        assert_eq!(tasks[1].state, "failed");
        assert_eq!(tasks[2].state, "blocked");
        assert_eq!(b.failed_ids(), vec!["slack.render".to_string()]);
    }

    #[test]
    fn empty_board_renders_indeterminate() {
        let b = TaskBoard::default();
        assert_eq!(b.render().0, None);
        assert_eq!(b.render().1, None);
    }

    #[test]
    fn run_summary_is_authoritative() {
        let mut b = TaskBoard::default();
        feed(
            &mut b,
            &[
                r#"{"event":"run_plan","steps":["a","b"]}"#,
                r#"{"event":"step_start","step":"a","attempt":1}"#,
                // No step_finish for "a" (e.g. output raced a kill) —
                // the summary still settles it.
                r#"{"event":"run_summary","steps":[{"step":"a","status":"failed","attempts":1,"outputs":[]},{"step":"b","status":"skipped_up_to_date","attempts":0,"outputs":[]}]}"#,
            ],
        );
        let tasks = b.snapshot();
        assert_eq!(tasks[0].state, "failed");
        assert_eq!(tasks[1].state, "skipped");
    }
}
