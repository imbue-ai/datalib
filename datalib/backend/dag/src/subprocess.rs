//! `StepRun::Subprocess` execution.

use std::collections::BTreeMap;
use std::os::unix::process::CommandExt;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::Context;
use process_wrap::tokio::{CommandWrap, ProcessGroup};
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::events::{Event, EventSink, LogLevel, Stream};
use crate::step::{ArtifactState, FailureKind, StepCtx, StepError, StepOutcome};

/// Where a step finds the runner's pipe, named to the child in
/// `datalib_parent_watch::ENV_VAR`. Deliberately not stdin: a step is an
/// arbitrary program, and one that reads stdin expecting `/dev/null`'s
/// immediate end-of-file would block forever on a pipe nobody writes to.
const PARENT_PIPE_FD: libc::c_int = 3;

pub const ENV_STEP: &str = "DATALIB_DAG_STEP";
/// The run this invocation belongs to — the id every row of
/// `system/runs/runs.sqlite` carries — and which attempt of the step this is
/// within it (1 for the first). Stamp them into anything you write that
/// should be joinable back to the run.
pub const ENV_RUN_ID: &str = "DATALIB_DAG_RUN_ID";
pub const ENV_ATTEMPT: &str = "DATALIB_DAG_ATTEMPT";
/// The step's group id, its group's `type`, and its function — the two
/// halves the step id is composed from, plus the type. Only set for a
/// step declared under a `[[groups]]` entry; `ENV_GROUP_TYPE` only when
/// the group declares a type.
pub const ENV_GROUP: &str = "DATALIB_DAG_GROUP";
pub const ENV_GROUP_TYPE: &str = "DATALIB_DAG_GROUP_TYPE";
/// Under a diff group only: the group named by its `source` and that
/// group's `type` — whose raw store the step reads, and which renderer
/// it runs. Set by the loader in the step's `env` rather than by the
/// runner, so they are fingerprinted like any other env entry.
pub const ENV_SOURCE_GROUP: &str = "DATALIB_DAG_SOURCE_GROUP";
pub const ENV_SOURCE_GROUP_TYPE: &str = "DATALIB_DAG_SOURCE_GROUP_TYPE";
pub const ENV_FUNCTION: &str = "DATALIB_DAG_FUNCTION";
pub const ENV_DATA_ROOT: &str = "DATALIB_DAG_DATA_ROOT";
pub const ENV_INPUTS: &str = "DATALIB_DAG_INPUTS";
pub const ENV_CHANGED_INPUTS: &str = "DATALIB_DAG_CHANGED_INPUTS";
/// `StepCtx::reads` as a JSON object: input path → version.
pub const ENV_READS: &str = "DATALIB_READS";
/// Run-wide pinned timestamp (RFC 3339), set by the runner on every
/// step so all stamped outputs agree. Steps that record times should
/// prefer it over sampling their own clock.
pub const ENV_NOW: &str = "DATALIB_DAG_NOW";
/// Set by `datalib-dag --reset`, and then the step does no work: it
/// empties what the value names — `store`, or `blobs` for an ingest
/// step's store and its blob CAS with it — commits that, and exits. The
/// runner then forgets the step ever succeeded, so the next run does its
/// work from the start.
pub const ENV_RESET: &str = "DATALIB_DAG_RESET";
/// Seconds between a step's checkpoints, at most — see
/// `config::CheckpointCadence`.
pub const ENV_CHECKPOINT_CADENCE: &str = "DATALIB_DAG_CHECKPOINT_CADENCE";

/// The flag a step's params arrive on: the path of a JSON file holding
/// the entry's `params` subtree. A file rather than an argument because
/// params carry tokens, and argv is readable by every user on the
/// machine; the file is created `0600` and removed when the step exits.
pub const PARAMS_FILE_FLAG: &str = "--params-file";

/// Where the runner puts those files, under the data root.
pub const PARAMS_DIR_REL_PATH: &str = "system/params";

/// Write `json` to a fresh owner-only file under `<data_root>/system/params`.
/// The file lives as long as the returned handle.
pub fn write_params_file(
    data_root: &std::path::Path,
    step_id: &str,
    json: &str,
) -> anyhow::Result<tempfile::NamedTempFile> {
    use std::io::Write;
    let dir = data_root.join(PARAMS_DIR_REL_PATH);
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(&dir)
        .with_context(|| format!("create {}", dir.display()))?;
    // `tempfile` creates with `O_EXCL` and mode 0600, so the file is
    // never readable by anyone else, not even between create and write.
    let mut file = tempfile::Builder::new()
        .prefix(&format!("{}.", step_id.replace('/', "_")))
        .suffix(".json")
        .tempfile_in(&dir)
        .with_context(|| format!("create a params file in {}", dir.display()))?;
    file.write_all(json.as_bytes())
        .and_then(|()| file.flush())
        .with_context(|| format!("write {}", file.path().display()))?;
    Ok(file)
}

/// The final stdout line a subprocess step may emit.
#[derive(Debug, Default, Deserialize)]
struct WireOutcome {
    #[serde(default)]
    outputs: Vec<WireArtifactState>,
    /// Set (with a non-zero exit) to classify the failure.
    #[serde(default)]
    failure: Option<FailureKind>,
}

#[derive(Debug, Deserialize)]
struct WireArtifactState {
    path: crate::ArtifactPath,
    version: Option<String>,
    #[serde(default)]
    rows: Option<u64>,
}

impl WireOutcome {
    fn into_outputs(self, sink: &Arc<dyn EventSink>, step: &str) -> Vec<ArtifactState> {
        let mut out = Vec::with_capacity(self.outputs.len());
        for row in self.outputs {
            match row.version {
                Some(version) => out.push(ArtifactState {
                    path: row.path,
                    version,
                    rows: row.rows,
                }),
                None => sink.emit(&Event::Log {
                    step: step.to_string(),
                    level: LogLevel::Warn,
                    msg: format!(
                        "reported output {:?} with no version; content-hashing it \
                         instead. Steps report a content-derived version per \
                         output — see docs/dev/step_protocol.md",
                        row.path.as_str()
                    ),
                    ts: None,
                    stream: None,
                    target: None,
                    thread: None,
                    fields: None,
                }),
            }
        }
        out
    }
}

pub(crate) async fn run_subprocess(
    argv: &[String],
    env: &BTreeMap<String, String>,
    params: Option<&str>,
    extra_env: &BTreeMap<String, String>,
    attempt: u32,
    ctx: &StepCtx,
    sink: &Arc<dyn EventSink>,
) -> Result<StepOutcome, StepError> {
    let internal = |e: anyhow::Error| StepError::new(FailureKind::Data, e);

    let (prog, args) = argv
        .split_first()
        .ok_or_else(|| internal(anyhow::anyhow!("empty argv")))?;
    let inputs: Vec<&str> = ctx.inputs.iter().map(|a| a.as_str()).collect();
    let changed: Vec<&str> = ctx.changed_inputs.iter().map(|a| a.as_str()).collect();
    // Held until the child has exited: dropping it deletes the file.
    let params_file = match params {
        Some(json) => {
            Some(write_params_file(&ctx.data_root, &ctx.step_id, json).map_err(internal)?)
        }
        None => None,
    };
    let mut cmd = tokio::process::Command::new(prog);
    cmd.args(args);
    if let Some(f) = &params_file {
        cmd.arg(PARAMS_FILE_FLAG).arg(f.path());
    }
    for (key, value) in [
        (ENV_GROUP, &ctx.group),
        (ENV_GROUP_TYPE, &ctx.group_type),
        (ENV_FUNCTION, &ctx.function),
    ] {
        match value {
            Some(v) => cmd.env(key, v),
            // Unset for a step outside any group, even if the runner's own
            // environment carries one.
            None => cmd.env_remove(key),
        };
    }
    cmd.env(ENV_STEP, &ctx.step_id)
        .env(ENV_ATTEMPT, attempt.to_string())
        .env(ENV_DATA_ROOT, &ctx.data_root)
        .env(ENV_INPUTS, inputs.join("\n"))
        .env(ENV_CHANGED_INPUTS, changed.join("\n"))
        .env(
            ENV_READS,
            serde_json::to_string(&ctx.reads).expect("a string map is JSON"),
        )
        // Run-wide env from the runner (PATH with binary_dir
        // prepended, the pinned DATALIB_DAG_NOW, …); the step's
        // own `env:` entries win on key collision.
        .envs(extra_env)
        .envs(env)
        .current_dir(&ctx.data_root)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // If the runner dies (or a step future is dropped), don't
        // leave an orphaned download running.
        .kill_on_drop(true);

    // Every step is handed the runner's pipe, and reading EOF on it is
    // how a step notices a runner that died without running any code — a
    // SIGKILL, an abort, the OOM killer — the one case `kill_children`
    // cannot reach. A step that wants that watches the descriptor named
    // by `DATALIB_PARENT_PIPE` (`datalib_parent_watch::exit_with_parent`
    // does it in one call); a step that ignores it just carries one extra
    // open descriptor, which costs nothing.
    //
    // Not on stdin, though `Stdio::piped()` is how the pipe gets made: a
    // step is an arbitrary program, and one that reads stdin expecting
    // the immediate end-of-file `/dev/null` gives would block forever on
    // a pipe nobody writes to. The child moves the pipe to
    // `PARENT_PIPE_FD` and restores `/dev/null` on stdin before exec, so
    // a step that reads stdin sees exactly what it always did.
    //
    // The write end stays on the child handle: take it and the step reads
    // EOF at once and exits, believing the runner is already gone.
    cmd.env(datalib_parent_watch::ENV_VAR, PARENT_PIPE_FD.to_string())
        .stdin(Stdio::piped());
    let devnull = std::ffi::CString::new("/dev/null").expect("no interior nul");
    // Safety: only async-signal-safe calls may run between fork and exec.
    // `dup2`, `open` and `close` are; the `CString` is allocated above,
    // before the fork, and only its pointer is read here.
    unsafe {
        cmd.as_std_mut().pre_exec(move || {
            if libc::dup2(0, PARENT_PIPE_FD) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let null = libc::open(devnull.as_ptr(), libc::O_RDONLY);
            if null < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::dup2(null, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            if null != 0 {
                libc::close(null);
            }
            Ok(())
        });
    }
    // Its own process group, so a signal aimed at the step reaches what
    // the step spawned. A step is often a wrapper around something else
    // — `qmd_index` runs `node qmd embed` — and a `kill(pid)` the step
    // does not forward leaves that grandchild running after the runner
    // is gone. The cost is that a terminal's Ctrl-C no longer reaches
    // steps directly, which changes nothing: `interrupt_children` is
    // how the runner forwards it, and always was.
    //
    // `ProcessGroup::leader()` makes the step its group's leader, so the
    // group's id *is* the step's pid — which is what lets
    // `signal_children` reach a whole step from a pid alone. Its child
    // wrapper also reaps the rest of the group after the step exits,
    // which a bare `setpgid` does not.
    let mut child = CommandWrap::from(cmd)
        .wrap(ProcessGroup::leader())
        .spawn()
        .with_context(|| format!("spawn {prog:?}"))
        .map_err(internal)?;
    let _pid_guard = child.id().map(RegisteredChild::new);
    // Aborted when the step exits, so a rung is only ever sent to a group
    // whose leader is still there to be reaped.
    let stop_task = child.id().map(|pid| {
        let mut stop = ctx.stop.clone();
        tokio::spawn(async move {
            stop.requested().await;
            for (after, signal) in stop_ladder(stop.grace) {
                tokio::time::sleep(after).await;
                signal_group(pid, signal);
            }
        })
    });

    let stdout = child.stdout().take().expect("stdout piped");
    let stderr = child.stderr().take().expect("stderr piped");

    // Drain stderr concurrently: every line is forwarded onto the
    // event stream (so child chatter — tracing output, qmd noise — is
    // captured somewhere instead of discarded), and a short tail is
    // kept for the error message on failure. stderr is where commands
    // put ordinary progress chatter, so the default level is `info`;
    // structured tracing lines (JSON with a `level` field) keep their
    // own severity.
    let stderr_sink = sink.clone();
    let stderr_step = ctx.step_id.clone();
    let stderr_task = tokio::spawn(async move {
        let mut tail = ErrorTail::default();
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            let event = unwrap_line(&stderr_step, Stream::Stderr, &line);
            tail.consider(&event, &line);
            stderr_sink.emit(&event);
        }
        tail.join()
    });

    let mut outcome: Option<WireOutcome> = None;
    let mut lines = BufReader::new(stdout).lines();
    while let Some(line) = lines.next_line().await.map_err(|e| internal(e.into()))? {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(v) if v.get("event").and_then(|e| e.as_str()) == Some("outcome") => {
                match serde_json::from_value::<WireOutcome>(v) {
                    Ok(w) => outcome = Some(w),
                    Err(e) => {
                        return Err(internal(anyhow::anyhow!(
                            "step {}: malformed outcome line {line:?}: {e}",
                            ctx.step_id
                        )))
                    }
                }
            }
            Ok(v) => match serde_json::from_value::<Event>(v) {
                // Forward, re-tagged with the authoritative id.
                Ok(ev) => {
                    match &ev {
                        // A checkpoint goes to the scheduler, not to the
                        // event stream: it is the one event that changes
                        // what the runner does next, and the scheduler
                        // emits it once it has recorded it -- the same
                        // path an in-process step's seal takes, so a seal
                        // is announced once whichever kind of step made
                        // it. Forwarding it here as well put every
                        // subprocess seal on the stream twice, and the
                        // Manage screen counted four for a download that
                        // made two.
                        Event::Checkpoint { version, rows, .. } => {
                            match rows {
                                Some(n) => ctx.checkpoint_rows(version, *n),
                                None => ctx.checkpoint(version),
                            }
                            continue;
                        }
                        Event::Capabilities { streams_output, .. } => {
                            ctx.declare_streams_output(*streams_output)
                        }
                        _ => {}
                    }
                    sink.emit(&retag(ev, &ctx.step_id))
                }
                Err(_) => sink.emit(&unwrap_line(&ctx.step_id, Stream::Stdout, &line)),
            },
            Err(_) => sink.emit(&unwrap_line(&ctx.step_id, Stream::Stdout, &line)),
        }
    }

    let status = child
        .wait()
        .await
        .context("wait for subprocess")
        .map_err(internal)?;
    if let Some(t) = stop_task {
        t.abort();
    }
    let stderr_tail = stderr_task.await.unwrap_or_default();

    if status.success() {
        Ok(StepOutcome {
            outputs: outcome
                .map(|w| w.into_outputs(sink, &ctx.step_id))
                .unwrap_or_default(),
            exit: Some(status.into()),
        })
    } else {
        let w = outcome.unwrap_or_default();
        // One that ignored its SIGINT and was killed says nothing, but it
        // still ended because it was asked to.
        let asked = if ctx.stop.is_requested() {
            FailureKind::Cancelled
        } else {
            FailureKind::Data
        };
        let failure = w.failure.unwrap_or(asked);
        let outputs = w.into_outputs(sink, &ctx.step_id);
        let what = if failure == FailureKind::Cancelled {
            format!("step {} stopped when asked to", ctx.step_id)
        } else {
            format!("step {} exited with {status}", ctx.step_id)
        };
        Err(StepError::new(
            failure,
            anyhow::anyhow!(
                "{what}{}{}",
                if stderr_tail.is_empty() { "" } else { ": " },
                stderr_tail
            ),
        )
        .with_outputs(outputs)
        .with_exit(status.into()))
    }
}

/// The part of a step's stderr that belongs in its error message. Every
/// line is already in the run store; the message is what a person reads
/// on the Manage row's hover, so it keeps only what they could not guess
/// from "it failed": plain lines (a panic, a shell's complaint) and the
/// message of a structured warn/error line. A structured info line — the
/// bulk of a tracing stream — is dropped, JSON envelope and all.
#[derive(Default)]
struct ErrorTail {
    lines: Vec<String>,
}

impl ErrorTail {
    const KEEP: usize = 8;

    fn consider(&mut self, event: &Event, raw: &str) {
        let Event::Log { level, msg, .. } = event else {
            return;
        };
        let structured = msg != raw;
        if structured && *level == LogLevel::Info {
            return;
        }
        if self.lines.len() == Self::KEEP {
            self.lines.remove(0);
        }
        self.lines.push(msg.clone());
    }

    fn join(&self) -> String {
        self.lines.join("\n")
    }
}

/// Keys of tracing-subscriber's JSON envelope that become columns of the
/// event rather than entries in `fields`. Everything else the envelope
/// carries — `filename`, `line_number`, `spans` — stays in `fields`.
const ENVELOPE_LIFTED: &[&str] = &[
    "timestamp",
    "level",
    "target",
    "threadId",
    "threadName",
    "fields",
];

/// A forwarded line from either pipe. Structured tracing output (JSON
/// with a `level` field, e.g. tracing-subscriber's JSON format) is
/// unwrapped — its message, severity, target, thread and timestamp
/// become the event's, and its other fields ride along as `fields` — so
/// nothing downstream parses an envelope out of a string. Everything
/// else — progress bars, plain chatter — is the line itself at `info`.
fn unwrap_line(step: &str, stream: Stream, line: &str) -> Event {
    let plain = || Event::Log {
        step: step.to_string(),
        level: LogLevel::Info,
        msg: line.to_string(),
        ts: None,
        stream: Some(stream),
        target: None,
        thread: None,
        fields: None,
    };
    let Ok(serde_json::Value::Object(mut env)) = serde_json::from_str::<serde_json::Value>(line)
    else {
        return plain();
    };
    let Some(level_word) = env.get("level").and_then(|l| l.as_str()) else {
        return plain();
    };
    // tracing-subscriber spells the level in capitals; `warning` is
    // what a Python step's logging module writes.
    let level = match level_word.to_ascii_lowercase().as_str() {
        "warning" => LogLevel::Warn,
        word => LogLevel::parse(word).unwrap_or(LogLevel::Info),
    };
    let string_at = |key: &str| env.get(key).and_then(|v| v.as_str()).map(str::to_string);
    let ts = string_at("timestamp");
    let target = string_at("target");
    // The name when the subscriber recorded one ("tokio-runtime-worker",
    // "run-store"), else the id ("ThreadId(7)").
    let thread = string_at("threadName").or_else(|| string_at("threadId"));
    let mut fields = match env.remove("fields") {
        Some(serde_json::Value::Object(f)) => f,
        _ => serde_json::Map::new(),
    };
    // `message` is the ordinary case; `event` is what the providers use
    // for their structured "this happened" records, which have no prose
    // at all — the event name is the sentence.
    let msg = ["message", "event"]
        .iter()
        .find_map(|k| {
            fields
                .remove(*k)
                .and_then(|v| v.as_str().map(str::to_string))
        })
        .unwrap_or_else(|| line.to_string());
    // A line bridged from the `log` crate carries `log.file`,
    // `log.line`, `log.module_path` and `log.target` beside the
    // envelope's own `filename`, `line_number` and `target`.
    fields.retain(|k, _| !k.starts_with("log."));
    for (k, v) in env {
        if !ENVELOPE_LIFTED.contains(&k.as_str()) {
            fields.entry(k).or_insert(v);
        }
    }
    Event::Log {
        step: step.to_string(),
        level,
        msg,
        ts,
        stream: Some(stream),
        target,
        thread,
        fields: (!fields.is_empty()).then_some(fields),
    }
}

fn retag(ev: Event, id: &str) -> Event {
    let id = id.to_string();
    match ev {
        Event::StepStart {
            attempt, builtin, ..
        } => Event::StepStart {
            step: id,
            attempt,
            builtin,
        },
        Event::StepFinish {
            status,
            error,
            exit_code,
            signal,
            ..
        } => Event::StepFinish {
            step: id,
            status,
            error,
            exit_code,
            signal,
        },
        Event::PassEnd {
            exit_code, signal, ..
        } => Event::PassEnd {
            step: id,
            exit_code,
            signal,
        },
        Event::Checkpoint { version, rows, .. } => Event::Checkpoint {
            step: id,
            version,
            rows,
        },
        Event::Capabilities { streams_output, .. } => Event::Capabilities {
            step: id,
            streams_output,
        },
        Event::ProgressLength { total, .. } => Event::ProgressLength { step: id, total },
        Event::ProgressInc { delta, .. } => Event::ProgressInc { step: id, delta },
        Event::ProgressMessage { msg, .. } => Event::ProgressMessage { step: id, msg },
        Event::Metric {
            name,
            labels,
            value,
            ..
        } => Event::Metric {
            step: id,
            name,
            labels,
            value,
        },
        Event::Log {
            level,
            msg,
            ts,
            stream,
            target,
            thread,
            fields,
            ..
        } => Event::Log {
            step: id,
            level,
            msg,
            ts,
            stream,
            target,
            thread,
            fields,
        },
        Event::Hint { msg, .. } => Event::Hint { step: id, msg },
        // through unmodified rather than inventing a policy.
        ev @ (Event::RunSummary { .. } | Event::RunPlan { .. }) => ev,
    }
}

// ── Child registry + signal forwarding ───────────────────────────────

static CHILD_PIDS: std::sync::Mutex<Option<std::collections::HashSet<u32>>> =
    std::sync::Mutex::new(None);

struct RegisteredChild(u32);

impl RegisteredChild {
    fn new(pid: u32) -> Self {
        CHILD_PIDS
            .lock()
            .unwrap()
            .get_or_insert_with(Default::default)
            .insert(pid);
        Self(pid)
    }
}

impl Drop for RegisteredChild {
    fn drop(&mut self) {
        if let Some(set) = CHILD_PIDS.lock().unwrap().as_mut() {
            set.remove(&self.0);
        }
    }
}

/// SIGINT every running step and what it spawned, so each exits
/// `stopped`.
pub fn interrupt_children() {
    #[cfg(unix)]
    signal_children(libc::SIGINT);
}

/// SIGKILL every running step and what it spawned. For the exits
/// `std::process::exit` takes, where no `kill_on_drop` runs: a step that
/// ignored its SIGINT must not outlive the runner, holding its store
/// open.
///
/// A runner that is itself SIGKILLed never reaches this. What covers
/// that is the other end: each step holds a pipe from the runner and
/// stops itself when it reads EOF, so this is the tidy path rather than
/// the only one.
pub fn kill_children() {
    #[cfg(unix)]
    signal_children(libc::SIGKILL);
}

#[cfg(unix)]
fn signal_children(signal: libc::c_int) {
    let pids: Vec<u32> = CHILD_PIDS
        .lock()
        .unwrap()
        .as_ref()
        .map(|s| s.iter().copied().collect())
        .unwrap_or_default();
    for pid in pids {
        signal_group(pid, signal);
    }
}

/// What a stopped step is sent, and how long after the previous rung:
/// SIGINT at once, so it stops at its next consistent point
/// (`step_protocol.md` § Signals), and SIGKILL once `grace` has passed,
/// for one that ignored it. Both to the group, so what the step spawned
/// goes too.
fn stop_ladder(grace: std::time::Duration) -> [(std::time::Duration, libc::c_int); 2] {
    [
        (std::time::Duration::ZERO, libc::SIGINT),
        (grace, libc::SIGKILL),
    ]
}

/// The step's process *group*, not the step: `spawn` gives each one a
/// group of its own, so its id is the step's pid and this reaches
/// whatever the step spawned as well as the step.
fn signal_group(pid: u32, signal: libc::c_int) {
    // Safety: plain kill(2) with a valid signal; racing a just-exited
    // group is benign (ESRCH).
    unsafe {
        libc::kill(-(pid as libc::pid_t), signal);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;
    use crate::graph::Graph;
    use crate::scheduler::{Runner, StepStatus};
    use crate::step::{StepRun, StepSpec};

    #[derive(Default)]
    struct Recorder(Mutex<Vec<Event>>);
    impl EventSink for Recorder {
        fn emit(&self, event: &Event) {
            self.0.lock().unwrap().push(event.clone());
        }
    }

    fn sh(script: &str) -> StepRun {
        StepRun::Subprocess {
            argv: vec!["/bin/sh".into(), "-c".into(), script.into()],
            env: BTreeMap::new(),
            params: None,
        }
    }

    /// Whether a pid still names a live process. Signal 0 checks without
    /// sending anything.
    #[cfg(unix)]
    fn alive(pid: libc::pid_t) -> bool {
        // Safety: plain kill(2) with the null signal.
        unsafe { libc::kill(pid, 0) == 0 }
    }

    /// Wait for what the next assertion is about, so a hang names what
    /// never arrived instead of timing out silently.
    #[cfg(unix)]
    async fn until(what: &str, mut ready: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            if ready() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("timed out waiting for {what}");
    }

    /// Every step gets `/dev/null` on stdin and the runner's pipe on fd
    /// 3. Both halves matter.
    ///
    /// Stdin, because a step is an arbitrary program: one that reads it
    /// expecting the immediate end-of-file `/dev/null` gives would block
    /// forever on a pipe nobody writes to, and a hung step holding its
    /// store open is worse than the orphan the pipe prevents. The `cat`
    /// is that assertion — if stdin were the pipe it never returns, the
    /// step never finishes, and this times out rather than failing on the
    /// recorded line.
    ///
    /// Fd 3, because that is what `datalib_parent_watch` is pointed at,
    /// and it refuses to start if the variable names something that is
    /// not a pipe — so the number and the descriptor have to agree.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_step_gets_dev_null_on_stdin_and_the_parent_pipe_beside_it() {
        let root = tempfile::tempdir().unwrap();
        let spec = StepSpec::new(
            "g/out",
            sh(r#"
                out="$DATALIB_DAG_DATA_ROOT/g/out"
                mkdir -p "$out"
                cat > /dev/null
                if [ -p /dev/fd/0 ]; then stdin=pipe; else stdin=not-a-pipe; fi
                if [ -p /dev/fd/3 ]; then watch=pipe; else watch=no-pipe; fi
                echo "$stdin $watch ${DATALIB_PARENT_PIPE:-unset}" > "$out/fds"
            "#),
        );
        let g = Graph::build(vec![spec]).unwrap();
        let data_root = root.path().to_path_buf();
        Runner::new(data_root).run(&g).await.expect("the run");

        assert_eq!(
            std::fs::read_to_string(root.path().join("g/out/fds"))
                .expect("the step wrote what it saw")
                .trim(),
            "not-a-pipe pipe 3",
        );
    }

    /// A signal aimed at a step reaches what the step spawned, because
    /// the runner gives each step a process group of its own and
    /// `signal_children` signals the group. Without it a `qmd embed`
    /// outlived the app that started it: the step never forwarded the
    /// signal, and nothing else ever signals a step.
    ///
    /// The kill below is `signal_children`'s exact call. If a step were
    /// left in the runner's own group there would be no group whose id
    /// is the step's pid, the kill would be ESRCH, and the grandchild
    /// would still be running when the deadline passed.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_signal_to_a_step_reaches_what_the_step_spawned() {
        let root = tempfile::tempdir().unwrap();
        let out = root.path().join("g/out");
        let spec = StepSpec::new(
            "g/out",
            sh(r#"
                out="$DATALIB_DAG_DATA_ROOT/g/out"
                mkdir -p "$out"
                echo $$ > "$out/step.pid"
                sleep 120 &
                # Written last, so the test polling for it knows both are
                # on disk.
                echo $! > "$out/grandchild.pid"
                while :; do sleep 0.2; done
            "#),
        );
        let g = Graph::build(vec![spec]).unwrap();
        let data_root = root.path().to_path_buf();
        let runner = tokio::spawn(async move { Runner::new(data_root).run(&g).await });

        let pid = |name: &str| -> Option<libc::pid_t> {
            std::fs::read_to_string(out.join(name))
                .ok()?
                .trim()
                .parse()
                .ok()
        };
        until("the step to spawn a child of its own", || {
            pid("step.pid").is_some() && pid("grandchild.pid").is_some()
        })
        .await;
        let step = pid("step.pid").unwrap();
        let grandchild = pid("grandchild.pid").unwrap();
        assert!(alive(grandchild), "the spawned child should be running");

        // Safety: plain kill(2) on the group the runner just created.
        unsafe { libc::kill(-step, libc::SIGKILL) };

        until("the step's own child to go with it", || !alive(grandchild)).await;
        let _ = runner.await;
    }

    /// Stopping a round stops each running step through its own process
    /// group, and nothing else starts: not the source still waiting on the
    /// budget, not the render below the stopped one.
    ///
    /// The step's `sleep` is in the foreground, and `sh` runs its INT trap
    /// only once that has exited. A SIGINT to the step's pid alone would
    /// leave the sleep running and the round waiting on it for two minutes.
    #[cfg(unix)]
    #[tokio::test]
    async fn stopping_a_round_interrupts_its_steps_and_starts_nothing_else() {
        let root = tempfile::tempdir().unwrap();
        let started = root.path().join("started");
        std::fs::create_dir_all(&started).unwrap();
        let source = |id: &str| {
            StepSpec::new(
                id,
                sh(r#"
                    trap 'echo "{\"event\":\"outcome\",\"failure\":\"cancelled\"}"; exit 130' INT
                    touch "$DATALIB_DAG_DATA_ROOT/started/${DATALIB_DAG_STEP%/*}"
                    sleep 120
                "#),
            )
        };
        let render = StepSpec::new("a/rendered", sh("touch started/render")).input("a/raw");
        let g = Graph::build(vec![source("a/raw"), source("b/raw"), render]).unwrap();

        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let mut runner = Runner::new(root.path()).stop_on(stop_rx);
        runner
            .lock_slots
            .insert(crate::supervisor::locks::NETWORK.into(), 1);
        let round = tokio::spawn(async move { runner.run(&g).await });

        let count = || std::fs::read_dir(&started).unwrap().count();
        until("a source to start", || count() == 1).await;
        stop.send(true).unwrap();
        let rep = tokio::time::timeout(std::time::Duration::from_secs(20), round)
            .await
            .expect("the stop reached the step's sleep")
            .unwrap()
            .unwrap();

        assert_eq!(count(), 1, "only the first source ever started");
        let cancelled = StepStatus::Failed {
            kind: FailureKind::Cancelled,
        };
        for id in ["a/raw", "b/raw", "a/rendered"] {
            assert_eq!(rep.step(id).status, cancelled, "{id}: {rep:#?}");
        }
    }

    /// A stopped step that ignores its SIGINT is killed once the grace is
    /// up, with what it spawned, and recorded as stopped. Without the
    /// second rung a step like this held its store, and the loop waited
    /// on it, for as long as it cared to run.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_step_deaf_to_its_stop_is_killed_after_the_grace() {
        let root = tempfile::tempdir().unwrap();
        let out = root.path().join("g/deaf");
        let spec = StepSpec::new(
            "g/deaf",
            sh(r#"
                trap '' INT
                out="$DATALIB_DAG_DATA_ROOT/g/deaf"
                mkdir -p "$out"
                sleep 120 &
                echo $! > "$out/grandchild.pid"
                while :; do sleep 0.1; done
            "#),
        );
        let g = Graph::build(vec![spec]).unwrap();
        let (stop, stop_rx) = tokio::sync::watch::channel(false);
        let mut runner = Runner::new(root.path()).stop_on(stop_rx);
        runner.stop_grace = std::time::Duration::from_millis(300);
        let round = tokio::spawn(async move { runner.run(&g).await });

        let grandchild = || -> Option<libc::pid_t> {
            std::fs::read_to_string(out.join("grandchild.pid"))
                .ok()?
                .trim()
                .parse()
                .ok()
        };
        until("the step to spawn a child of its own", || {
            grandchild().is_some()
        })
        .await;
        let child = grandchild().unwrap();
        stop.send(true).unwrap();
        let rep = tokio::time::timeout(std::time::Duration::from_secs(20), round)
            .await
            .expect("the kill ended the round")
            .unwrap()
            .unwrap();

        assert_eq!(
            rep.step("g/deaf").status,
            StepStatus::Failed {
                kind: FailureKind::Cancelled
            }
        );
        until("the step's child to go with it", || !alive(child)).await;
    }

    /// A step is told the version of each input it was started against, as
    /// the runner recorded it — which is how the qmd index versions itself
    /// by what it indexed.
    #[tokio::test]
    async fn a_step_is_told_the_versions_it_was_started_against() {
        let root = tempfile::tempdir().unwrap();
        let producer = StepSpec::new(
            "src/raw",
            sh(r#"mkdir -p src/raw && echo data > src/raw/f
                  echo '{"event":"outcome","outputs":[{"path":"src/raw","version":"v7"}]}'"#),
        );
        let consumer = StepSpec::new(
            "src/rendered",
            sh(r#"mkdir -p src/rendered && printf '%s' "$DATALIB_READS" > src/rendered/reads.json"#),
        )
        .input("src/raw");
        let g = Graph::build(vec![producer, consumer]).unwrap();
        let rep = Runner::new(root.path()).run(&g).await.unwrap();
        assert!(rep.all_ok(), "{rep:#?}");

        let reads: BTreeMap<String, String> = serde_json::from_str(
            &std::fs::read_to_string(root.path().join("src/rendered/reads.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            reads,
            BTreeMap::from([(
                "src/raw".to_string(),
                rep.step("src/raw").outputs[0].1.clone()
            )])
        );
        assert!(reads["src/raw"].ends_with(":v7"), "{reads:?}");
    }

    /// Params reach the child as a file only its owner can read, named
    /// on `--params-file`, and the file is gone once the step exits.
    #[tokio::test]
    async fn params_arrive_in_an_owner_only_file_that_does_not_outlive_the_step() {
        let root = tempfile::tempdir().unwrap();
        let spec = StepSpec::new(
            "p/out",
            StepRun::Subprocess {
                argv: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    // `$0` is the script name; the runner appends the
                    // flag and the path after it, so they land in $1 $2.
                    r#"
                        mkdir -p p/out
                        [ "$1" = "--params-file" ] || exit 3
                        stat -f '%Lp' "$2" > p/out/mode.txt 2>/dev/null || stat -c '%a' "$2" > p/out/mode.txt
                        cp "$2" p/out/params.json
                    "#
                    .into(),
                    "sh".into(),
                ],
                env: BTreeMap::new(),
                params: Some(r#"{"token":"s3cret"}"#.into()),
            },
        );
        let g = Graph::build(vec![spec]).unwrap();
        let rep = Runner::new(root.path()).run(&g).await.unwrap();
        assert!(rep.all_ok(), "{rep:#?}");
        assert_eq!(
            std::fs::read_to_string(root.path().join("p/out/params.json")).unwrap(),
            r#"{"token":"s3cret"}"#
        );
        assert_eq!(
            std::fs::read_to_string(root.path().join("p/out/mode.txt"))
                .unwrap()
                .trim(),
            "600"
        );
        let leftover: Vec<_> = std::fs::read_dir(root.path().join(super::PARAMS_DIR_REL_PATH))
            .unwrap()
            .collect();
        assert!(
            leftover.is_empty(),
            "params file outlived the step: {leftover:?}"
        );
    }

    /// A step written against the older protocol reports
    /// `{"path": …, "changed": true}` and no version. That must not fail
    /// the step: the row is dropped with a warning, and the success gets a
    /// version of its own, new each time, which is what `changed: true`
    /// meant.
    #[tokio::test]
    async fn outcome_row_without_a_version_warns_and_gets_a_fresh_version() {
        let root = tempfile::tempdir().unwrap();
        let spec = StepSpec::new(
            "legacy/raw",
            sh(r#"
                mkdir -p "$DATALIB_DAG_DATA_ROOT/legacy/raw"
                echo body > "$DATALIB_DAG_DATA_ROOT/legacy/raw/x.txt"
                echo '{"event":"outcome","outputs":[{"path":"legacy/raw","changed":true}]}'
            "#),
        );
        let g = Graph::build(vec![spec]).unwrap();
        let rec = Arc::new(Recorder::default());
        let rep = Runner::new(root.path())
            .sink(rec.clone())
            .run(&g)
            .await
            .unwrap();

        assert!(
            rep.all_ok(),
            "a versionless row must not fail the step: {rep:#?}"
        );
        let version = &rep.step("legacy/raw").outputs[0].1;
        assert!(version.contains(":run-"), "{version}");

        let warned = rec.0.lock().unwrap().iter().any(|e| {
            matches!(e, Event::Log { level: LogLevel::Warn, msg, .. } if msg.contains("no version"))
        });
        assert!(warned, "the step author needs to hear about it");
    }

    #[tokio::test]
    async fn a_subprocess_declares_streaming_on_the_wire_and_a_consumer_runs_early() {
        let root = tempfile::tempdir().unwrap();
        let passes = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));

        // A real child process: the capability and the checkpoints cross
        // stdout as NDJSON, which is the path a `datalib-step` render takes.
        // It re-announces until the consumer has run, because a checkpoint
        // arriving while the consumer is busy is dropped by design.
        let producer = StepSpec::new(
            "slack/rendered_md",
            sh(r#"
                out="$DATALIB_DAG_DATA_ROOT/slack/rendered_md"
                mkdir -p "$out"
                echo rows > "$out/data.md"
                echo '{"event":"capabilities","step":"me","streams_output":true}'
                i=0
                while [ $i -lt 200 ]; do
                    echo '{"event":"checkpoint","step":"me","version":"v1","rows":7}'
                    if [ -f "$DATALIB_DAG_DATA_ROOT/consumed" ]; then break; fi
                    sleep 0.05
                    i=$((i+1))
                done
                echo '{"event":"outcome","outputs":[{"path":"slack/rendered_md","version":"final","rows":3}]}'
            "#),
        );

        let consumer = {
            let passes = passes.clone();
            StepSpec::new(
                "unified_index/grid",
                StepRun::in_process(move |ctx: crate::step::StepCtx| {
                    let passes = passes.clone();
                    async move {
                        let dir = ctx.path_str(&ctx.step_id);
                        std::fs::create_dir_all(&dir).unwrap();
                        std::fs::write(dir.join("index.txt"), "x").unwrap();
                        passes.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        // Tells the producer it may stop re-announcing.
                        std::fs::write(ctx.data_root.join("consumed"), "1").unwrap();
                        Ok(crate::step::StepOutcome::default())
                    }
                }),
            )
            .input("slack/rendered_md")
        };

        let g = Graph::build(vec![producer, consumer]).unwrap();
        let rec = Arc::new(Recorder::default());
        let rep = tokio::time::timeout(
            std::time::Duration::from_secs(30),
            Runner::new(root.path()).sink(rec.clone()).run(&g),
        )
        .await
        .expect("a declared-streaming subprocess must not deadlock")
        .unwrap();

        assert!(rep.all_ok(), "{rep:#?}");
        // One early pass driven by the wire checkpoint, plus the final
        // pass for the version no checkpoint reported.
        assert!(
            passes.load(std::sync::atomic::Ordering::SeqCst) >= 2,
            "the consumer never ran early: the capability or the checkpoint \
             did not survive the subprocess boundary"
        );
        // The row counts crossed the wire too: the checkpoint's 7 and
        // the outcome's 3 both reached the consumer's queue.
        let queued: Vec<i64> = rec
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                Event::Metric {
                    step, name, value, ..
                } if step == "unified_index/grid" && name == "queued" => Some(*value),
                _ => None,
            })
            .collect();
        // Exact, because the producer waits for the early pass before it
        // finishes: the seal's 7, drained by that pass; the outcome's 3,
        // drained by the final one. The re-announced seal counts once.
        assert_eq!(queued, vec![7, 0, 3, 0]);
    }

    /// A seal a child announces on stdout reaches the event stream once.
    /// It used to arrive twice -- forwarded from the wire, and again from
    /// the scheduler when the signal reached it -- so every subprocess
    /// step's `checkpoints` metric read double, and the Manage screen
    /// counted four seals for a download that made two.
    #[tokio::test]
    async fn a_subprocess_checkpoint_reaches_the_event_stream_once() {
        let root = tempfile::tempdir().unwrap();
        let spec = StepSpec::new(
            "slack/rendered_md",
            sh(r#"
                out="$DATALIB_DAG_DATA_ROOT/slack/rendered_md"
                mkdir -p "$out"
                echo rows > "$out/data.md"
                echo '{"event":"capabilities","step":"me","streams_output":true}'
                echo '{"event":"checkpoint","step":"me","version":"v1","rows":7}'
                echo '{"event":"outcome","outputs":[{"path":"slack/rendered_md","version":"final"}]}'
            "#),
        );
        let g = Graph::build(vec![spec]).unwrap();
        let rec = Arc::new(Recorder::default());
        let rep = Runner::new(root.path())
            .sink(rec.clone())
            .run(&g)
            .await
            .unwrap();
        assert!(rep.all_ok(), "{rep:#?}");
        let seals: Vec<(String, Option<u64>)> = rec
            .0
            .lock()
            .unwrap()
            .iter()
            .filter_map(|e| match e {
                Event::Checkpoint { version, rows, .. } => Some((version.clone(), *rows)),
                _ => None,
            })
            .collect();
        assert_eq!(seals, vec![("v1".to_string(), Some(7))]);
    }

    #[tokio::test]
    async fn subprocess_step_events_outcome_and_env() {
        let root = tempfile::tempdir().unwrap();
        let spec = StepSpec::new(
            "shell/raw",
            sh(r#"
                mkdir -p "$DATALIB_DAG_DATA_ROOT/shell/raw"
                echo "hi from $DATALIB_DAG_STEP" > "$DATALIB_DAG_DATA_ROOT/shell/raw/x.txt"
                echo '{"event":"progress_message","step":"me","msg":"halfway"}'
                echo plain text line
                echo "downloading 3/10..." >&2
                echo '{"timestamp":"2026-09-11T08:00:00.000Z","level":"ERROR","target":"slack::ingest","threadName":"main","threadId":"ThreadId(1)","filename":"x.rs","fields":{"message":"boom","channel":"C1"}}' >&2
                echo '{"timestamp":"2026-09-11T08:00:01.000Z","level":"DEBUG","target":"datalib_etl::doltlite_raw","fields":{"message":"committed"}}' >&2
                echo '{"event":"outcome","outputs":[{"path":"shell/raw","version":"v1"}]}'
            "#),
        );
        let g = Graph::build(vec![spec]).unwrap();
        let rec = Arc::new(Recorder::default());
        let r = Runner::new(root.path()).sink(rec.clone());
        let rep = r.run(&g).await.unwrap();

        assert!(rep.all_ok(), "{rep:#?}");
        assert_eq!(
            std::fs::read_to_string(root.path().join("shell/raw/x.txt")).unwrap(),
            "hi from shell/raw\n"
        );
        // The reported version was trusted verbatim.
        // The recorded version is the step's fingerprint plus what it
        // reported, so a change to the step itself reaches consumers.
        assert!(
            rep.step("shell/raw").outputs[0].1.ends_with(":v1"),
            "{}",
            rep.step("shell/raw").outputs[0].1
        );

        let events = rec.0.lock().unwrap();
        // Progress event forwarded and re-tagged from "me" to the real id.
        assert!(events.iter().any(|e| matches!(
            e,
            Event::ProgressMessage { step, msg } if step == "shell/raw" && msg == "halfway"
        )));
        // Plain text forwarded as a log line.
        assert!(events.iter().any(|e| matches!(
            e,
            Event::Log { msg, .. } if msg == "plain text line"
        )));
        // stderr chatter defaults to info; structured tracing lines
        // keep their own severity.
        assert!(events.iter().any(|e| matches!(
            e,
            Event::Log { level: LogLevel::Info, msg, .. } if msg == "downloading 3/10..."
        )));
        // A tracing envelope is unwrapped: message, target and the
        // leftover fields become columns, and the location noise is gone.
        let boom = events
            .iter()
            .find(|e| {
                matches!(
                    e,
                    Event::Log {
                        level: LogLevel::Error,
                        ..
                    }
                )
            })
            .expect("the tracing error line reaches the stream");
        match boom {
            Event::Log {
                msg,
                ts,
                stream,
                target,
                thread,
                fields,
                ..
            } => {
                assert_eq!(msg, "boom");
                assert_eq!(ts.as_deref(), Some("2026-09-11T08:00:00.000Z"));
                assert_eq!(*stream, Some(Stream::Stderr));
                assert_eq!(target.as_deref(), Some("slack::ingest"));
                assert_eq!(thread.as_deref(), Some("main"), "the name beats the id");
                let fields = fields.as_ref().expect("the extra fields survive");
                assert_eq!(fields["channel"], "C1");
                assert_eq!(
                    fields["filename"], "x.rs",
                    "where it came from stays, as a field"
                );
                assert!(!fields.contains_key("threadId"), "lifted, not repeated");
            }
            _ => unreachable!(),
        }
        // A `debug` line is kept as one, not rounded up to `info`: the
        // step's default filter passes debug so the store can hold it.
        assert!(events.iter().any(|e| matches!(
            e,
            Event::Log { level: LogLevel::Debug, msg, .. } if msg == "committed"
        )));
        // A plain line says which pipe it came from and nothing more.
        assert!(events.iter().any(|e| matches!(
            e,
            Event::Log { msg, stream: Some(Stream::Stdout), ts: None, .. } if msg == "plain text line"
        )));
        assert!(events.iter().any(|e| matches!(
            e,
            Event::Log { msg, stream: Some(Stream::Stderr), .. } if msg == "downloading 3/10..."
        )));
    }

    #[tokio::test]
    async fn child_env_reaches_steps_and_step_env_wins() {
        let root = tempfile::tempdir().unwrap();
        let spec = StepSpec::new(
            "env/out",
            StepRun::Subprocess {
                argv: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    r#"
                        mkdir -p env/out
                        echo "$DATALIB_DAG_NOW/$OVERRIDE_ME" > env/out/probe.txt
                    "#
                    .into(),
                ],
                env: [("OVERRIDE_ME".to_string(), "step".to_string())].into(),
                params: None,
            },
        );
        let g = Graph::build(vec![spec]).unwrap();
        let r = Runner::new(root.path()).child_env(
            [
                (
                    super::ENV_NOW.to_string(),
                    "2026-07-21T00:00:00Z".to_string(),
                ),
                ("OVERRIDE_ME".to_string(), "run".to_string()),
            ]
            .into(),
        );
        let rep = r.run(&g).await.unwrap();
        assert!(rep.all_ok(), "{rep:#?}");
        assert_eq!(
            std::fs::read_to_string(root.path().join("env/out/probe.txt")).unwrap(),
            "2026-07-21T00:00:00Z/step\n",
            "run-wide env is visible; the step's own env wins on collision"
        );
    }

    /// `--reset` invokes the step with `DATALIB_DAG_RESET` naming the
    /// part, and forgets the step's last success, so the next run runs it
    /// again with nothing marked as changed.
    #[tokio::test]
    async fn a_reset_invokes_the_step_with_the_part_and_forgets_its_success() {
        let root = tempfile::tempdir().unwrap();
        let spec = StepSpec::new(
            "src/raw",
            sh(r#"
                mkdir -p src/raw
                echo "${DATALIB_DAG_RESET:-run}" >> src/raw/log.txt
            "#),
        );
        let g = Graph::build(vec![spec]).unwrap();
        let log = root.path().join("src/raw/log.txt");
        let r = Runner::new(root.path());
        assert!(r.run(&g).await.unwrap().all_ok());
        assert!(crate::supervisor::record::recorded(root.path()).await.steps["src/raw"].succeeded);

        r.reset(&g, &[crate::scheduler::ResetTarget::parse("src/raw+blobs")])
            .await
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(&log).unwrap(),
            "run\nblobs\n",
            "the reset invocation names the part and does nothing else"
        );
        let after = crate::supervisor::record::recorded(root.path())
            .await
            .steps
            .remove("src/raw")
            .unwrap_or_default();
        assert_eq!(
            (after.succeeded, after.last_run, after.last_success_at),
            (false, None, None),
            "a reset step has never succeeded"
        );
        let err = r
            .reset(&g, &[crate::scheduler::ResetTarget::parse("nope/raw")])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("no such step"), "{err:#}");
    }

    /// The error message a stopped or failed step leaves behind is what
    /// the Manage row shows on hover. Structured info lines — the
    /// tracing stream a step writes as JSON — stay in the run store and
    /// out of the message; a warning's text and a plain line stay in.
    #[test]
    fn error_tail_keeps_prose_and_drops_structured_info() {
        let mut tail = ErrorTail::default();
        let lines = [
            r#"{"timestamp":"2026-09-18T20:16:06Z","level":"INFO","fields":{"message":"walked one messages.list page","page":2},"target":"gmail"}"#,
            r#"{"timestamp":"2026-09-18T20:24:07Z","level":"WARN","fields":{"message":"interrupt checkpoint: store busy"},"target":"datalib_step"}"#,
            "429 too many requests",
        ];
        for line in lines {
            tail.consider(&unwrap_line("s", Stream::Stderr, line), line);
        }
        assert_eq!(
            tail.join(),
            "interrupt checkpoint: store busy\n429 too many requests"
        );

        let mut tail = ErrorTail::default();
        for i in 0..(ErrorTail::KEEP * 2) {
            let line = format!("line {i}");
            tail.consider(&unwrap_line("s", Stream::Stderr, &line), &line);
        }
        assert_eq!(tail.lines.len(), ErrorTail::KEEP);
        assert_eq!(tail.lines.last().map(String::as_str), Some("line 15"));
    }

    /// A step that answers SIGINT with a `cancelled` outcome is told
    /// apart from one that fell over: its message says it stopped, not
    /// that it exited with 130.
    #[tokio::test]
    async fn a_cancelled_subprocess_says_it_stopped() {
        let root = tempfile::tempdir().unwrap();
        let spec = StepSpec::new(
            "gmail/ingest",
            sh(r#"
                echo '{"level":"INFO","fields":{"message":"interrupt checkpoint: ok"}}' >&2
                echo '{"event":"outcome","failure":"cancelled"}'
                exit 130
            "#),
        );
        let g = Graph::build(vec![spec]).unwrap();
        let rep = Runner::new(root.path()).run(&g).await.unwrap();
        let step = rep.step("gmail/ingest");
        assert_eq!(
            step.status,
            StepStatus::Failed {
                kind: FailureKind::Cancelled
            }
        );
        assert_eq!(
            step.error.as_deref(),
            Some("step gmail/ingest stopped when asked to")
        );
    }

    #[tokio::test]
    async fn subprocess_failure_classification_and_stderr_tail() {
        let root = tempfile::tempdir().unwrap();
        let spec = StepSpec::new(
            "bad/raw",
            sh(r#"
                echo '{"event":"outcome","failure":"rate_limited"}'
                echo "429 too many requests" >&2
                exit 3
            "#),
        );
        let g = Graph::build(vec![spec]).unwrap();
        // One retry round-trip happens (rate_limited is retryable) —
        // zero backoff keeps the test fast.
        let r = Runner::new(root.path()).retry(crate::scheduler::RetryPolicy {
            backoff: std::time::Duration::ZERO,
            rate_limited_attempts: 2,
            ..Default::default()
        });
        let rep = r.run(&g).await.unwrap();
        let step = rep.step("bad/raw");
        assert_eq!(
            step.status,
            StepStatus::Failed {
                kind: FailureKind::RateLimited
            }
        );
        assert_eq!(step.attempts, 2);
        assert!(
            step.error
                .as_deref()
                .unwrap()
                .contains("429 too many requests"),
            "{:?}",
            step.error
        );
    }
}
