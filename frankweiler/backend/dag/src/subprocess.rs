//! `StepRun::Subprocess` execution.
//!
//! The wire protocol is the Unix-y one from the design doc: the child
//! writes NDJSON on **stdout** — the same [`Event`] schema the
//! in-process sinks use, plus one final `{"event": "outcome", ...}`
//! line carrying its [`StepOutcome`] (or failure classification).
//! No ports, no registration, no per-child auth token.
//!
//! * Progress events are re-tagged with the authoritative step id and
//!   forwarded to the orchestrator's sink.
//! * Unparseable stdout lines are forwarded as info logs (so a step
//!   can be a plain shell command that prints text).
//! * stderr is captured and its tail becomes the error message on a
//!   non-zero exit.
//! * Exit 0 with no outcome line → success with no output report (the
//!   scheduler content-hashes). Non-zero exit → failure; the kind
//!   comes from the outcome line if the child wrote one, else `Data`.
//!
//! The child learns its identity from the environment:
//! `FRANKWEILER_DAG_STEP`, `FRANKWEILER_DAG_DATA_ROOT`,
//! `FRANKWEILER_DAG_INPUTS` (all resolved input artifacts,
//! `\n`-separated, relative to the data root) and
//! `FRANKWEILER_DAG_CHANGED_INPUTS` (the subset whose version moved).

use std::collections::BTreeMap;
use std::process::Stdio;
use std::sync::Arc;

use anyhow::Context;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::events::{Event, EventSink, LogLevel};
use crate::step::{ArtifactState, FailureKind, StepCtx, StepError, StepOutcome};

pub const ENV_STEP: &str = "FRANKWEILER_DAG_STEP";
pub const ENV_DATA_ROOT: &str = "FRANKWEILER_DAG_DATA_ROOT";
pub const ENV_INPUTS: &str = "FRANKWEILER_DAG_INPUTS";
pub const ENV_CHANGED_INPUTS: &str = "FRANKWEILER_DAG_CHANGED_INPUTS";

/// The final stdout line a subprocess step may emit.
#[derive(Debug, Default, Deserialize)]
struct WireOutcome {
    #[serde(default)]
    outputs: Vec<ArtifactState>,
    /// Set (with a non-zero exit) to classify the failure.
    #[serde(default)]
    failure: Option<FailureKind>,
}

pub(crate) async fn run_subprocess(
    argv: &[String],
    env: &BTreeMap<String, String>,
    ctx: &StepCtx,
    sink: &Arc<dyn EventSink>,
) -> Result<StepOutcome, StepError> {
    let internal = |e: anyhow::Error| StepError::new(FailureKind::Data, e);

    let (prog, args) = argv
        .split_first()
        .ok_or_else(|| internal(anyhow::anyhow!("empty argv")))?;
    let inputs: Vec<&str> = ctx.inputs.iter().map(|a| a.as_str()).collect();
    let changed: Vec<&str> = ctx.changed_inputs.iter().map(|a| a.as_str()).collect();
    let mut cmd = tokio::process::Command::new(prog);
    cmd.args(args)
        .env(ENV_STEP, &ctx.step_id)
        .env(ENV_DATA_ROOT, &ctx.data_root)
        .env(ENV_INPUTS, inputs.join("\n"))
        .env(ENV_CHANGED_INPUTS, changed.join("\n"))
        .envs(env)
        .current_dir(&ctx.data_root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // If the runner dies (or a step future is dropped), don't
        // leave an orphaned download running.
        .kill_on_drop(true);
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {prog:?}"))
        .map_err(internal)?;
    let _pid_guard = child.id().map(RegisteredChild::new);

    let stdout = child.stdout.take().expect("stdout piped");
    let stderr = child.stderr.take().expect("stderr piped");

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
        let mut tail: Vec<String> = Vec::new();
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            stderr_sink.emit(&Event::Log {
                step: stderr_step.clone(),
                level: stderr_level(&line),
                msg: line.clone(),
            });
            tail.push(line);
            if tail.len() > 20 {
                tail.remove(0);
            }
        }
        tail.join("\n")
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
                Ok(ev) => sink.emit(&retag(ev, &ctx.step_id)),
                Err(_) => forward_text(sink, ctx, &line),
            },
            Err(_) => forward_text(sink, ctx, &line),
        }
    }

    let status = child
        .wait()
        .await
        .context("wait for subprocess")
        .map_err(internal)?;
    let stderr_tail = stderr_task.await.unwrap_or_default();

    if status.success() {
        Ok(StepOutcome {
            outputs: outcome.map(|w| w.outputs).unwrap_or_default(),
        })
    } else {
        let w = outcome.unwrap_or_default();
        Err(StepError::new(
            w.failure.unwrap_or(FailureKind::Data),
            anyhow::anyhow!(
                "step {} exited with {status}{}{}",
                ctx.step_id,
                if stderr_tail.is_empty() { "" } else { ": " },
                stderr_tail
            ),
        )
        .with_outputs(w.outputs))
    }
}

/// Severity for a forwarded stderr line. Structured tracing output
/// (JSON with a `level` field, e.g. tracing-subscriber's JSON format)
/// keeps its own WARN/ERROR; everything else — progress bars, plain
/// chatter, even JSON INFO lines — is `info`.
fn stderr_level(line: &str) -> LogLevel {
    let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
        return LogLevel::Info;
    };
    match v.get("level").and_then(|l| l.as_str()) {
        Some(l) if l.eq_ignore_ascii_case("warn") || l.eq_ignore_ascii_case("warning") => {
            LogLevel::Warn
        }
        Some(l) if l.eq_ignore_ascii_case("error") => LogLevel::Error,
        _ => LogLevel::Info,
    }
}

fn forward_text(sink: &Arc<dyn EventSink>, ctx: &StepCtx, line: &str) {
    sink.emit(&Event::Log {
        step: ctx.step_id.clone(),
        level: LogLevel::Info,
        msg: line.to_string(),
    });
}

/// Replace whatever step id the child claimed with the real one.
fn retag(ev: Event, id: &str) -> Event {
    let id = id.to_string();
    match ev {
        Event::StepStart { attempt, .. } => Event::StepStart { step: id, attempt },
        Event::StepFinish { status, error, .. } => Event::StepFinish {
            step: id,
            status,
            error,
        },
        Event::ProgressLength { total, .. } => Event::ProgressLength { step: id, total },
        Event::ProgressInc { delta, .. } => Event::ProgressInc { step: id, delta },
        Event::ProgressMessage { msg, .. } => Event::ProgressMessage { step: id, msg },
        Event::Log { level, msg, .. } => Event::Log {
            step: id,
            level,
            msg,
        },
        Event::Hint { msg, .. } => Event::Hint { step: id, msg },
        // through unmodified rather than inventing a policy.
        ev @ (Event::RunSummary { .. } | Event::RunPlan { .. }) => ev,
    }
}

// ── Child registry + signal forwarding ───────────────────────────────
//
// Terminal Ctrl-C reaches the whole foreground process group, but a
// programmatic cancel (the http worker killing the runner) signals
// only the runner. The registry lets the runner's signal handler
// forward SIGINT to every live child so steps get their chance to
// checkpoint-commit before exiting.

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

/// Send SIGINT to every live step subprocess (best effort). Unix only;
/// elsewhere a no-op.
pub fn interrupt_children() {
    #[cfg(unix)]
    {
        let pids: Vec<u32> = CHILD_PIDS
            .lock()
            .unwrap()
            .as_ref()
            .map(|s| s.iter().copied().collect())
            .unwrap_or_default();
        for pid in pids {
            // Safety: plain kill(2) with a valid signal; racing a
            // just-exited pid is benign (ESRCH).
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGINT);
            }
        }
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
        }
    }

    #[tokio::test]
    async fn subprocess_step_events_outcome_and_env() {
        let root = tempfile::tempdir().unwrap();
        let spec = StepSpec::new(
            "shell.download",
            sh(r#"
                mkdir -p "$FRANKWEILER_DAG_DATA_ROOT/shell/raw"
                echo "hi from $FRANKWEILER_DAG_STEP" > "$FRANKWEILER_DAG_DATA_ROOT/shell/raw/x.txt"
                echo '{"event":"progress_message","step":"me","msg":"halfway"}'
                echo plain text line
                echo "downloading 3/10..." >&2
                echo '{"timestamp":"t","level":"ERROR","fields":{"message":"boom"}}' >&2
                echo '{"event":"outcome","outputs":[{"path":"shell/raw","changed":true,"version":"v1"}]}'
            "#),
        )
        .output("shell/raw");
        let g = Graph::build(vec![spec]).unwrap();
        let rec = Arc::new(Recorder::default());
        let r = Runner::new(root.path()).sink(rec.clone());
        let rep = r.run(&g).await.unwrap();

        assert!(rep.all_ok(), "{rep:#?}");
        assert_eq!(
            std::fs::read_to_string(root.path().join("shell/raw/x.txt")).unwrap(),
            "hi from shell.download\n"
        );
        // The reported version was trusted verbatim.
        assert_eq!(rep.step("shell.download").outputs[0].1, "v1");

        let events = rec.0.lock().unwrap();
        // Progress event forwarded and re-tagged from "me" to the real id.
        assert!(events.iter().any(|e| matches!(
            e,
            Event::ProgressMessage { step, msg } if step == "shell.download" && msg == "halfway"
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
        assert!(events.iter().any(|e| matches!(
            e,
            Event::Log { level: LogLevel::Error, msg, .. } if msg.contains("boom")
        )));
    }

    #[tokio::test]
    async fn subprocess_failure_classification_and_stderr_tail() {
        let root = tempfile::tempdir().unwrap();
        let spec = StepSpec::new(
            "bad.download",
            sh(r#"
                echo '{"event":"outcome","failure":"rate_limited"}'
                echo "429 too many requests" >&2
                exit 3
            "#),
        )
        .output("bad/raw");
        let g = Graph::build(vec![spec]).unwrap();
        // One retry round-trip happens (rate_limited is retryable) —
        // zero backoff keeps the test fast.
        let r = Runner::new(root.path()).retry(crate::scheduler::RetryPolicy {
            backoff: std::time::Duration::ZERO,
            rate_limited_attempts: 2,
            ..Default::default()
        });
        let rep = r.run(&g).await.unwrap();
        let step = rep.step("bad.download");
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
