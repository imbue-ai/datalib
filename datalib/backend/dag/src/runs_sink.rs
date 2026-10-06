//! Publishing the event stream to the run store.
//!
//! Step state is kept here and written whole: the store coalesces to the
//! newest row per step, so every row it sees has to be complete. The
//! `progress_length` / `progress_inc` sugar is also resolved here — the
//! wire carries deltas, the store carries positions, and coalescing
//! deltas would lose work.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, Weak};

use datalib_runs::store::{now_split, split_stamp};
use datalib_runs::{
    canonical_labels, LiveState, LogRow, MetricRow, Process, ProcessRow, Retention, RunWriter,
    StepRunRow,
};

use crate::events::{Event, EventSink, LogLevel};
use crate::run_state::RunState;
use crate::step::StepId;

/// What we know about one step right now.
#[derive(Default, Clone)]
struct Acc {
    row: StepRunRow,
    /// From the plan; every line about the step carries it.
    group: Option<String>,
    /// The attempt's process: what came out of the step's pipes is its,
    /// and how it ended goes on it.
    process: Option<ProcessRow>,
    /// The sugar accumulators: increments so far, and the announced total.
    done: u64,
    total: Option<u64>,
    checkpoints: u64,
    /// The labels of every `queued` series written for the step, so its
    /// finish can empty them all.
    queued: BTreeSet<String>,
    /// This attempt's last value of each running total (`name`, labels),
    /// and whether it has already been caught going down.
    counters: HashMap<(String, String), (i64, bool)>,
}

/// Record a running total's new value, and say what it was if this is
/// the first time within the attempt it has gone down.
fn counter_fell(
    counters: &mut HashMap<(String, String), (i64, bool)>,
    name: &str,
    labels: &str,
    value: i64,
) -> Option<i64> {
    let entry = counters
        .entry((name.to_string(), labels.to_string()))
        .or_insert((value, false));
    let (last, warned) = *entry;
    *entry = (value, warned || value < last);
    (value < last && !warned).then_some(last)
}

/// An [`EventSink`] that keeps the run store current.
pub struct RunStoreSink {
    writer: Arc<RunWriter>,
    steps: Mutex<HashMap<StepId, Acc>>,
    /// The runner's commit, which a built-in step's attempt shares.
    git_hash: Option<String>,
}

/// The store's clock: UTC, with the runner's offset beside it.
fn now() -> (String, Option<String>) {
    now_split()
}

/// The `target` of a line the runner writes about a step — a
/// checkpoint sealed, why the step ended, a hint — so a filter on
/// target finds the runner's lines the way it finds a crate's.
const RUNNER_TARGET: &str = "datalib_dag::runner";

/// The gauge of work still ahead of a step: its own, and one per
/// producer (`queued{from=<producer>}`) that the scheduler keeps.
const QUEUED: &str = datalib_metrics::QUEUED;

impl RunStoreSink {
    /// Returns `None` when the store could not be opened. The store is
    /// observability: a sync that runs without a record is much better
    /// than one that refuses to start because a status file was
    /// unwritable.
    pub fn start(
        data_root: &std::path::Path,
        run_id: &str,
        started_at_utc: &str,
        git_hash: Option<String>,
        retention: Retention,
    ) -> Option<Self> {
        Some(Self {
            writer: Arc::new(RunWriter::start(
                data_root,
                run_id,
                started_at_utc,
                git_hash.clone(),
                retention,
            )?),
            steps: Mutex::new(HashMap::new()),
            git_hash,
        })
    }

    /// For the runner's own `tracing` lines, through
    /// [`datalib_runs::StoreLayer`]: they land in the run with no step.
    /// Weak, so the global subscriber never keeps the writer — and its
    /// final flush — from happening when this sink is dropped.
    pub fn log_sink(&self) -> Weak<RunWriter> {
        Arc::downgrade(&self.writer)
    }

    fn update(&self, step: &StepId, f: impl FnOnce(&mut Acc)) {
        let row = {
            let mut steps = self.steps.lock().expect("run store sink mutex");
            let acc = steps.entry(step.clone()).or_default();
            f(acc);
            acc.row.step = step.clone();
            let (at, offset) = now();
            acc.row.updated_at_utc = at;
            acc.row.tz_offset = offset;
            acc.row.clone()
        };
        self.writer.step(row);
    }

    fn metric(&self, step: &StepId, name: &str, labels: &BTreeMap<String, String>, value: i64) {
        let labels = canonical_labels(labels);
        let fell_from = {
            let mut steps = self.steps.lock().expect("run store sink mutex");
            let acc = steps.entry(step.clone()).or_default();
            if name == QUEUED {
                acc.queued.insert(labels.clone());
            }
            if datalib_metrics::is_counter(name) {
                counter_fell(&mut acc.counters, name, &labels, value)
            } else {
                None
            }
        };
        if let Some(before) = fell_from {
            self.warn_counter_fell(step, name, &labels, before, value);
        }
        self.write_metric(step, name, labels, value);
    }

    /// A running total that went down is a step naming a gauge as a
    /// counter: `GET /metrics` would serve it as a counter, and a scraper
    /// would read each fall as a reset and count the climb back twice.
    fn warn_counter_fell(&self, step: &StepId, name: &str, labels: &str, before: i64, now: i64) {
        let series = if labels.is_empty() {
            name.to_string()
        } else {
            format!("{name}{{{labels}}}")
        };
        let (ts_utc, tz_offset) = now_split();
        self.writer.log(LogRow {
            step: Some(step.clone()),
            group_id: self.group_of(step),
            attempt: self.attempt_of(step),
            ts_utc,
            tz_offset,
            level: LogLevel::Warn.as_str().into(),
            target: Some(RUNNER_TARGET.into()),
            msg: format!(
                "{series} went down, {before} to {now}, but a name ending in `_total` says it only \
                 grows; name it without the suffix if it can fall (docs/dev/step_protocol.md)"
            ),
            fields: Some(
                serde_json::json!({ "counter_fell": series, "from": before, "to": now })
                    .to_string(),
            ),
            ..Default::default()
        });
    }

    fn write_metric(&self, step: &StepId, name: &str, labels: String, value: i64) {
        let (updated_at_utc, tz_offset) = now();
        self.writer.metric(MetricRow {
            step: step.clone(),
            name: name.to_string(),
            labels,
            value,
            updated_at_utc,
            tz_offset,
            ..Default::default()
        });
    }

    fn attempt_of(&self, step: &StepId) -> i64 {
        self.steps
            .lock()
            .expect("run store sink mutex")
            .get(step)
            .map(|a| a.row.attempt)
            .unwrap_or(0)
    }

    fn group_of(&self, step: &StepId) -> Option<String> {
        self.steps
            .lock()
            .expect("run store sink mutex")
            .get(step)
            .and_then(|a| a.group.clone())
    }

    /// The process of the step's current attempt, for a line that came
    /// out of it; empty — the writer's own — for a step never started.
    fn process_of(&self, step: &StepId) -> String {
        self.steps
            .lock()
            .expect("run store sink mutex")
            .get(step)
            .and_then(|a| a.process.as_ref())
            .map(|p| p.process_id.clone())
            .unwrap_or_default()
    }

    /// The sugar: `done` is the increments so far, `queued` what the
    /// announced total leaves. A step that never announced a total gets
    /// `done` alone, which a bar cannot be drawn from and a count can.
    fn publish_sugar(&self, step: &StepId) {
        let (done, total) = {
            let steps = self.steps.lock().expect("run store sink mutex");
            let acc = steps.get(step).cloned().unwrap_or_default();
            (acc.done, acc.total)
        };
        let none = BTreeMap::new();
        self.metric(step, datalib_metrics::DONE, &none, done as i64);
        if let Some(total) = total {
            self.metric(step, QUEUED, &none, total.saturating_sub(done) as i64);
        }
    }
}

impl EventSink for RunStoreSink {
    fn emit(&self, event: &Event) {
        match event {
            // Publish the whole plan up front so a reader can draw every
            // row, pending ones included, before anything has started.
            Event::RunPlan { steps } => {
                for planned in steps {
                    self.update(&planned.step, |a| {
                        a.row.state = LiveState::Pending.as_str().into();
                        a.group = planned.group.clone();
                    });
                }
            }
            // A retry re-runs the step from zero, so the counters reset
            // with it — otherwise attempt 2 would appear to start
            // wherever attempt 1 died.
            Event::StepStart {
                step,
                attempt,
                builtin,
            } => {
                let (started_at_utc, tz_offset) = now();
                let process = ProcessRow {
                    process_id: datalib_runs::new_process_id(),
                    process: Process::Step.as_str().into(),
                    step: Some(step.clone()),
                    attempt: Some(*attempt as i64),
                    started_at_utc: started_at_utc.clone(),
                    tz_offset,
                    git_hash: builtin.then(|| self.git_hash.clone()).flatten(),
                    ..Default::default()
                };
                self.writer.process(process.clone());
                // `counters` goes with the rest: a retry counts from zero.
                self.update(step, |a| {
                    *a = Acc {
                        queued: std::mem::take(&mut a.queued),
                        row: StepRunRow {
                            state: LiveState::Running.as_str().into(),
                            attempt: *attempt as i64,
                            started_at_utc: Some(started_at_utc),
                            ..Default::default()
                        },
                        process: Some(process),
                        group: a.group.take(),
                        ..Default::default()
                    }
                })
            }
            // The reason a step ended badly is a log line too, so the
            // log panel has a line to jump to: the last error-level line
            // of a failed step is why it failed, the last warn-level one
            // of a stopped step is where it stopped.
            Event::StepFinish {
                step,
                status,
                error,
                exit_code,
                signal,
            } => {
                let finished_at_utc = now().0;
                let mut ended = None;
                let mut queued = BTreeSet::new();
                self.update(step, |a| {
                    queued = std::mem::take(&mut a.queued);
                    a.row.state = status.as_str().into();
                    a.row.finished_at_utc = Some(finished_at_utc.clone());
                    a.row.error = error.clone();
                    // A pass that ended while its producers ran was closed
                    // by its `PassEnd`; the finish only settles the row.
                    if let Some(p) = a.process.as_mut().filter(|p| p.finished_at_utc.is_none()) {
                        p.finished_at_utc = Some(finished_at_utc.clone());
                        p.exit_code = exit_code.map(i64::from);
                        p.signal = signal.map(i64::from);
                        ended = Some(p.clone());
                    }
                });
                if let Some(p) = ended {
                    self.writer.process(p);
                }
                // Ended is ended, however: nothing is ahead of it now.
                for labels in queued {
                    self.write_metric(step, QUEUED, labels, 0);
                }
                if let Some(error) = error {
                    let level = if *status == RunState::Stopped {
                        LogLevel::Warn
                    } else {
                        LogLevel::Error
                    };
                    let (ts_utc, tz_offset) = now();
                    self.writer.log(LogRow {
                        step: Some(step.clone()),
                        group_id: self.group_of(step),
                        attempt: self.attempt_of(step),
                        ts_utc,
                        tz_offset,
                        level: level.as_str().into(),
                        target: Some(RUNNER_TARGET.into()),
                        msg: error.clone(),
                        fields: Some(
                            serde_json::json!({ "finished": status.as_str() }).to_string(),
                        ),
                        ..Default::default()
                    });
                }
            }
            // The pass's process is closed the way a finished attempt's
            // is; the step row is not touched, since the step goes on.
            Event::PassEnd {
                step,
                exit_code,
                signal,
            } => {
                let finished_at_utc = now().0;
                let mut ended = None;
                self.update(step, |a| {
                    if let Some(p) = a.process.as_mut() {
                        p.finished_at_utc = Some(finished_at_utc.clone());
                        p.exit_code = exit_code.map(i64::from);
                        p.signal = signal.map(i64::from);
                        ended = Some(p.clone());
                    }
                });
                if let Some(p) = ended {
                    self.writer.process(p);
                }
            }
            Event::Metric {
                step,
                name,
                labels,
                value,
            } => self.metric(step, name, labels, *value),
            Event::ProgressLength { step, total } => {
                self.update(step, |a| a.total = *total);
                self.publish_sugar(step);
            }
            Event::ProgressInc { step, delta } => {
                self.update(step, |a| a.done = a.done.saturating_add(*delta));
                self.publish_sugar(step);
            }
            Event::ProgressMessage { step, msg } => {
                self.update(step, |a| a.row.msg = Some(msg.clone()))
            }
            // A checkpoint is the one thing a watcher can act on before
            // the step ends, so it is a log line as well as a count.
            Event::Checkpoint {
                step,
                version,
                rows,
            } => {
                let n = {
                    let mut steps = self.steps.lock().expect("run store sink mutex");
                    let acc = steps.entry(step.clone()).or_default();
                    acc.checkpoints += 1;
                    acc.checkpoints
                };
                self.metric(
                    step,
                    datalib_metrics::CHECKPOINTS,
                    &BTreeMap::new(),
                    n as i64,
                );
                let (ts_utc, tz_offset) = now();
                let since_last = match rows {
                    Some(rows) => format!("{rows} rows since the last, "),
                    None => String::new(),
                };
                self.writer.log(LogRow {
                    step: Some(step.clone()),
                    group_id: self.group_of(step),
                    attempt: self.attempt_of(step),
                    ts_utc,
                    tz_offset,
                    level: LogLevel::Info.as_str().into(),
                    target: Some(RUNNER_TARGET.into()),
                    msg: format!("sealed checkpoint #{n}: {since_last}now at {version}"),
                    fields: Some(
                        serde_json::json!({ "version": version, "rows": rows, "checkpoint": n })
                            .to_string(),
                    ),
                    ..Default::default()
                });
            }
            Event::Log {
                step,
                level,
                msg,
                ts,
                stream,
                target,
                thread,
                fields,
            } => {
                // The line's own clock when it has one — that is when the
                // step wrote it, where ours is when we read it.
                let (ts_utc, tz_offset) = match ts {
                    Some(own) => split_stamp(own),
                    None => now(),
                };
                // A line from the step's pipe is the step's; one the
                // runner wrote about the step is the runner's.
                let process_id = match stream {
                    Some(_) => self.process_of(step),
                    None => String::new(),
                };
                self.writer.log(LogRow {
                    step: Some(step.clone()),
                    group_id: self.group_of(step),
                    attempt: self.attempt_of(step),
                    process_id,
                    ts_utc,
                    tz_offset,
                    stream: stream.map(|s| s.as_str().to_string()),
                    level: level.as_str().into(),
                    // A line with no stream came from the runner itself,
                    // not out of a pipe; the runner is then its target.
                    target: target
                        .clone()
                        .or_else(|| stream.is_none().then(|| RUNNER_TARGET.into())),
                    thread: thread.clone(),
                    msg: msg.clone(),
                    fields: fields
                        .as_ref()
                        .map(|f| serde_json::Value::Object(f.clone()).to_string()),
                    ..Default::default()
                })
            }
            Event::Hint { step, msg } => {
                let (ts_utc, tz_offset) = now();
                self.writer.log(LogRow {
                    step: Some(step.clone()),
                    group_id: self.group_of(step),
                    attempt: self.attempt_of(step),
                    ts_utc,
                    tz_offset,
                    level: LogLevel::Warn.as_str().into(),
                    target: Some(RUNNER_TARGET.into()),
                    msg: msg.clone(),
                    fields: Some(r#"{"hint":true}"#.into()),
                    ..Default::default()
                })
            }
            // Nothing to record: a capability is about what the step
            // *can* do, and the summary repeats what `StepFinish` said.
            Event::Capabilities { .. } | Event::RunSummary { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::PlannedStep;
    use datalib_runs::{
        log_after, log_query, processes, snapshot, LogCursor, LogQuery, Snapshot, Stream,
    };

    async fn run(events: &[Event]) -> (tempfile::TempDir, Snapshot) {
        let td = tempfile::tempdir().unwrap();
        {
            let sink = RunStoreSink::start(
                td.path(),
                "run-1",
                "2026-09-11T10:00:00+01:00",
                Some("ae2d52f0".into()),
                Retention::default(),
            )
            .expect("start the store");
            for e in events {
                sink.emit(e);
            }
        } // dropping the sink drops the writer, which flushes and joins
        let snap = snapshot(td.path()).await;
        (td, snap)
    }

    fn metric_value(snap: &Snapshot, step: &str, name: &str) -> Option<i64> {
        snap.metrics
            .iter()
            .find(|m| m.step == step && m.name == name)
            .map(|m| m.value)
    }

    fn inc(step: &str, delta: u64) -> Event {
        Event::ProgressInc {
            step: step.into(),
            delta,
        }
    }

    /// The sugar. The stream carries increments; the store must carry a
    /// position, or coalescing would drop work.
    /// The runner is a process the run points at, and every line the
    /// run stores reads the runner's commit through it.
    #[tokio::test]
    async fn the_run_and_its_lines_name_the_runners_process() {
        let (td, snap) = run(&[Event::Log {
            step: "slack/raw".into(),
            level: LogLevel::Info,
            msg: "hello".into(),
            ts: None,
            stream: None,
            target: None,
            thread: None,
            fields: None,
        }])
        .await;
        let runner = processes(td.path(), None, None, 10)
            .await
            .into_iter()
            .find(|p| p.process == "dag")
            .expect("the runner is a process of the run");
        assert_eq!(runner.run_id.as_deref(), Some("run-1"));
        assert_eq!(runner.git_hash.as_deref(), Some("ae2d52f0"));
        assert_eq!(snap.run_id.as_deref(), Some("run-1"));
        let lines = log_after(td.path(), "run-1", None, 0, 10).await;
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].process_id, runner.process_id);
        assert_eq!(lines[0].process.as_deref(), Some("dag"));
        assert_eq!(lines[0].git_hash.as_deref(), Some("ae2d52f0"));
    }

    /// Every line about a step carries the group the plan filed it
    /// under — its own output, and what the runner said about it after
    /// `StepStart` reset the step's record — so `group:` finds a source's
    /// lines whatever step wrote them.
    #[tokio::test]
    async fn a_steps_lines_carry_its_group_from_the_plan() {
        let log = |step: &str, stream: Option<Stream>| Event::Log {
            step: step.into(),
            level: LogLevel::Info,
            msg: "hello".into(),
            ts: None,
            stream,
            target: None,
            thread: None,
            fields: None,
        };
        let (td, _snap) = run(&[
            Event::RunPlan {
                steps: vec![
                    PlannedStep {
                        step: "slack/ingest".into(),
                        group: Some("slack".into()),
                    },
                    PlannedStep {
                        step: "slack/render_markdown".into(),
                        group: Some("slack".into()),
                    },
                    PlannedStep {
                        step: "tools/backfill".into(),
                        group: None,
                    },
                ],
            },
            Event::StepStart {
                step: "slack/ingest".into(),
                attempt: 1,
                builtin: true,
            },
            log("slack/ingest", Some(Stream::Stderr)),
            Event::StepFinish {
                step: "slack/ingest".into(),
                status: RunState::Failed,
                error: Some("boom".into()),
                exit_code: Some(1),
                signal: None,
            },
            log("slack/render_markdown", None),
            log("tools/backfill", None),
        ])
        .await;
        let lines = log_after(td.path(), "run-1", None, 0, 10).await;
        let groups: Vec<(Option<&str>, Option<&str>)> = lines
            .iter()
            .map(|l| (l.step.as_deref(), l.group_id.as_deref()))
            .collect();
        assert_eq!(
            groups,
            [
                (Some("slack/ingest"), Some("slack")),
                (Some("slack/ingest"), Some("slack")),
                (Some("slack/render_markdown"), Some("slack")),
                (Some("tools/backfill"), None),
            ]
        );
        let slack = log_query(
            td.path(),
            &LogQuery {
                q: "group:slack",
                cursor: LogCursor::After(0),
                limit: 10,
            },
        )
        .await
        .unwrap();
        assert_eq!(slack.len(), 3);
    }

    /// A streaming consumer is spawned once per producer checkpoint,
    /// and every pass is a process that ends — with an exit code — while
    /// the step stays running until its final pass. Without `PassEnd`
    /// every pass but the last was left open, its exit code lost.
    #[tokio::test]
    async fn a_streaming_pass_closes_its_process_and_leaves_the_step_running() {
        let start = || Event::StepStart {
            step: "unified_index/grid".into(),
            attempt: 1,
            builtin: true,
        };
        let (td, snap) = run(&[
            start(),
            Event::PassEnd {
                step: "unified_index/grid".into(),
                exit_code: Some(0),
                signal: None,
            },
            start(),
        ])
        .await;
        let mut passes: Vec<_> = processes(td.path(), None, Some("step"), 10)
            .await
            .into_iter()
            .filter(|p| p.step.as_deref() == Some("unified_index/grid"))
            .collect();
        passes.sort_by(|a, b| a.started_at_utc.cmp(&b.started_at_utc));
        assert_eq!(passes.len(), 2, "one process per pass");
        assert_eq!(passes[0].exit_code, Some(0));
        assert!(passes[0].finished_at_utc.is_some(), "the first pass ended");
        assert!(passes[1].finished_at_utc.is_none(), "the second is running");
        let step = snap
            .steps
            .iter()
            .find(|s| s.step == "unified_index/grid")
            .expect("the step row");
        assert_eq!(step.state, "running");
    }

    /// Each attempt of a step is a process of the run: what came out of
    /// its pipes is its, the runner's commit is its when it runs the
    /// built-in program, and how it ended goes on it.
    #[tokio::test]
    async fn a_step_attempt_is_a_process_with_its_lines_and_its_exit() {
        let (td, _snap) = run(&[
            Event::StepStart {
                step: "slack/raw".into(),
                attempt: 1,
                builtin: true,
            },
            Event::Log {
                step: "slack/raw".into(),
                level: LogLevel::Info,
                msg: "from the pipe".into(),
                ts: None,
                stream: Some(Stream::Stderr),
                target: None,
                thread: None,
                fields: None,
            },
            Event::StepFinish {
                step: "slack/raw".into(),
                status: RunState::Failed,
                error: Some("exited 3".into()),
                exit_code: Some(3),
                signal: None,
            },
            Event::StepStart {
                step: "slack/raw".into(),
                attempt: 2,
                builtin: true,
            },
            Event::StepFinish {
                step: "slack/raw".into(),
                status: RunState::Succeeded,
                error: None,
                exit_code: Some(0),
                signal: None,
            },
        ])
        .await;
        let mut attempts: Vec<_> = processes(td.path(), None, None, 10)
            .await
            .into_iter()
            .filter(|p| p.process == "step")
            .collect();
        attempts.sort_by_key(|p| p.attempt);
        assert_eq!(attempts.len(), 2, "{attempts:?}");
        assert_eq!(attempts[0].step.as_deref(), Some("slack/raw"));
        assert_eq!(attempts[0].run_id.as_deref(), Some("run-1"));
        assert_eq!(attempts[0].exit_code, Some(3));
        assert!(attempts[0].finished_at_utc.is_some());
        assert_eq!(attempts[0].git_hash.as_deref(), Some("ae2d52f0"));
        assert_eq!(attempts[1].exit_code, Some(0));
        let lines = log_after(td.path(), "run-1", None, 0, 10).await;
        let piped = lines
            .iter()
            .find(|l| l.msg == "from the pipe")
            .expect("the pipe's line");
        assert_eq!(piped.process_id, attempts[0].process_id);
        assert_eq!(piped.process.as_deref(), Some("step"));
        // The runner's own line about the failure is the runner's.
        let reason = lines
            .iter()
            .find(|l| l.msg == "exited 3")
            .expect("the reason");
        assert_ne!(reason.process_id, attempts[0].process_id);
        assert_eq!(reason.process.as_deref(), Some("dag"));
    }

    #[tokio::test]
    async fn increments_accumulate_into_done_and_queued() {
        let (_td, snap) = run(&[
            Event::StepStart {
                step: "slack/raw".into(),
                attempt: 1,
                builtin: true,
            },
            Event::ProgressLength {
                step: "slack/raw".into(),
                total: Some(9),
            },
            inc("slack/raw", 1),
            inc("slack/raw", 3),
            inc("slack/raw", 2),
        ])
        .await;

        assert_eq!(snap.steps.len(), 1);
        assert_eq!(snap.steps[0].state, LiveState::Running.as_str());
        assert_eq!(
            metric_value(&snap, "slack/raw", datalib_metrics::DONE),
            Some(6),
            "1 + 3 + 2, not the last delta"
        );
        assert_eq!(metric_value(&snap, "slack/raw", "queued"), Some(3));
    }

    /// A step that has ended has nothing ahead of it, however it ended:
    /// its own `queued` and every `queued{from=…}` go to zero, the one
    /// published before it started included.
    #[tokio::test]
    async fn every_ending_empties_the_queue() {
        for status in [RunState::Succeeded, RunState::Failed, RunState::Stopped] {
            let (_td, snap) = run(&[
                Event::Metric {
                    step: "slack/raw".into(),
                    name: "queued".into(),
                    labels: BTreeMap::from([("from".to_string(), "slack/list".to_string())]),
                    value: 5,
                },
                Event::StepStart {
                    step: "slack/raw".into(),
                    attempt: 1,
                    builtin: true,
                },
                Event::ProgressLength {
                    step: "slack/raw".into(),
                    total: Some(9),
                },
                inc("slack/raw", 3),
                Event::StepFinish {
                    step: "slack/raw".into(),
                    status,
                    error: None,
                    exit_code: None,
                    signal: None,
                },
            ])
            .await;
            let queued: BTreeMap<String, i64> = snap
                .metrics
                .iter()
                .filter(|m| m.name == "queued")
                .map(|m| (m.labels.clone(), m.value))
                .collect();
            assert_eq!(
                queued,
                BTreeMap::from([(String::new(), 0), ("from=slack/list".to_string(), 0)]),
                "{status:?}"
            );
            assert_eq!(
                metric_value(&snap, "slack/raw", datalib_metrics::DONE),
                Some(3)
            );
        }
    }

    /// A retry re-runs the step from the beginning, so the count has to
    /// go back to zero. Otherwise attempt 2 appears to resume from
    /// wherever attempt 1 died and can sail past `total`.
    #[tokio::test]
    async fn a_retry_restarts_the_count() {
        let (_td, snap) = run(&[
            Event::StepStart {
                step: "slack/raw".into(),
                attempt: 1,
                builtin: true,
            },
            Event::ProgressLength {
                step: "slack/raw".into(),
                total: Some(9),
            },
            inc("slack/raw", 7),
            Event::StepStart {
                step: "slack/raw".into(),
                attempt: 2,
                builtin: true,
            },
            inc("slack/raw", 1),
        ])
        .await;

        assert_eq!(
            metric_value(&snap, "slack/raw", datalib_metrics::DONE),
            Some(1),
            "attempt 2 starts over, not at 8"
        );
        assert_eq!(snap.steps[0].attempt, 2);
    }

    /// A metric on the wire lands as written, labels and all, and a
    /// later value replaces an earlier one rather than adding to it.
    #[tokio::test]
    async fn metrics_are_absolute_and_labelled() {
        let table = |t: &str, v: i64| Event::Metric {
            step: "slack/raw".into(),
            name: "rows_upserted_total".into(),
            labels: BTreeMap::from([("table".to_string(), t.to_string())]),
            value: v,
        };
        let (_td, snap) = run(&[
            table("messages", 10),
            table("channels", 2),
            table("messages", 25),
        ])
        .await;
        let by_labels: BTreeMap<String, i64> = snap
            .metrics
            .iter()
            .map(|m| (m.labels.clone(), m.value))
            .collect();
        assert_eq!(
            by_labels,
            BTreeMap::from([
                ("table=channels".to_string(), 2),
                ("table=messages".to_string(), 25)
            ])
        );
    }

    /// A series named as a running total that goes down is a gauge
    /// under a counter's name, which `GET /metrics` would serve wrong: the
    /// runner says so in the step's log, once per series, and still
    /// records the value. A gauge going down says nothing.
    #[tokio::test]
    async fn a_running_total_that_goes_down_is_warned_about_once() {
        let m = |name: &str, v: i64| Event::Metric {
            step: "slack/raw".into(),
            name: name.into(),
            labels: BTreeMap::new(),
            value: v,
        };
        let (td, snap) = run(&[
            m("api_requests_total", 5),
            m("api_requests_total", 3),
            m("api_requests_total", 2),
            m("queued", 9),
            m("queued", 1),
        ])
        .await;
        assert_eq!(
            metric_value(&snap, "slack/raw", "api_requests_total"),
            Some(2)
        );
        let log = log_after(td.path(), "run-1", Some("slack/raw"), 0, 100).await;
        let warned: Vec<&str> = log
            .iter()
            .filter(|l| l.level == "warn")
            .map(|l| l.msg.as_str())
            .collect();
        assert_eq!(warned.len(), 1, "{warned:?}");
        assert!(
            warned[0].starts_with("api_requests_total went down, 5 to 3"),
            "{warned:?}"
        );
    }

    /// A reader must be able to draw the whole table before the first
    /// step starts, which is what makes the plan event worth publishing.
    #[tokio::test]
    async fn the_plan_lands_before_anything_runs() {
        let (_td, snap) = run(&[Event::RunPlan {
            steps: ["slack/raw", "slack/rendered_md"]
                .map(|step| PlannedStep {
                    step: step.into(),
                    group: Some("slack".into()),
                })
                .into(),
        }])
        .await;

        assert_eq!(snap.steps.len(), 2);
        assert!(snap
            .steps
            .iter()
            .all(|r| r.state == LiveState::Pending.as_str()));
        assert!(
            snap.metrics.is_empty(),
            "a step that has reported nothing has no numbers"
        );
    }

    /// The step's own words reach the store, and the terminal state
    /// sticks with its error and its finish time. The error is also the
    /// last line of the step's log, at error level, so the log panel
    /// has a line to jump to; a stop is a warning there, not an error.
    #[tokio::test]
    async fn the_last_message_and_the_outcome_are_recorded() {
        let (td, snap) = run(&[
            Event::StepStart {
                step: "slack/raw".into(),
                attempt: 1,
                builtin: true,
            },
            Event::ProgressMessage {
                step: "slack/raw".into(),
                msg: "conversations.list".into(),
            },
            Event::StepFinish {
                step: "slack/raw".into(),
                status: RunState::Failed,
                error: Some("boom".into()),
                exit_code: None,
                signal: None,
            },
        ])
        .await;

        let s = &snap.steps[0];
        assert_eq!(s.state, RunState::Failed.as_str());
        assert_eq!(s.msg.as_deref(), Some("conversations.list"));
        assert_eq!(s.error.as_deref(), Some("boom"));
        assert!(s.started_at_utc.is_some() && s.finished_at_utc.is_some());
        let log = log_after(td.path(), "run-1", Some("slack/raw"), 0, 100).await;
        assert_eq!(log.len(), 1, "{log:?}");
        assert_eq!(
            (log[0].level.as_str(), log[0].msg.as_str()),
            ("error", "boom")
        );
        assert_eq!(log[0].fields.as_deref(), Some(r#"{"finished":"failed"}"#));

        let (td, _) = run(&[
            Event::StepFinish {
                step: "slack/raw".into(),
                status: RunState::Stopped,
                error: Some("stopped when asked to".into()),
                exit_code: None,
                signal: None,
            },
            Event::StepFinish {
                step: "slack/rendered_md".into(),
                status: RunState::Succeeded,
                error: None,
                exit_code: None,
                signal: None,
            },
        ])
        .await;
        let log = log_after(td.path(), "run-1", None, 0, 100).await;
        assert_eq!(log.len(), 1, "a clean finish logs nothing: {log:?}");
        assert_eq!(log[0].level, "warn");
    }

    /// Log lines, hints and checkpoints all land in the log, unwrapped,
    /// and none of them disturbs the step's message line.
    #[tokio::test]
    async fn logs_hints_and_checkpoints_land_in_the_log() {
        let (td, snap) = run(&[
            Event::StepStart {
                step: "slack/raw".into(),
                attempt: 2,
                builtin: true,
            },
            Event::ProgressMessage {
                step: "slack/raw".into(),
                msg: "conversations.list".into(),
            },
            Event::Log {
                step: "slack/raw".into(),
                level: crate::events::LogLevel::Warn,
                msg: "rate limited, backing off".into(),
                ts: Some("2026-09-11T08:00:00Z".into()),
                stream: Some(crate::events::Stream::Stderr),
                target: Some("slack::http".into()),
                thread: Some("tokio-runtime-worker".into()),
                fields: Some(
                    serde_json::json!({"retry_in": 30})
                        .as_object()
                        .unwrap()
                        .clone(),
                ),
            },
            Event::Hint {
                step: "slack/raw".into(),
                msg: "run latchkey auth set slack".into(),
            },
            Event::Checkpoint {
                step: "slack/raw".into(),
                version: "abc".into(),
                rows: Some(12),
            },
        ])
        .await;

        assert_eq!(snap.steps[0].msg.as_deref(), Some("conversations.list"));
        assert_eq!(
            metric_value(&snap, "slack/raw", datalib_metrics::CHECKPOINTS),
            Some(1)
        );
        let log = log_after(td.path(), "run-1", Some("slack/raw"), 0, 100).await;
        assert_eq!(log.len(), 3, "{log:?}");
        assert_eq!(log[0].level, "warn");
        assert_eq!(
            log[0].ts_utc, "2026-09-11T08:00:00.000000+00:00",
            "the line's own clock wins, kept as UTC"
        );
        assert_eq!(log[0].tz_offset.as_deref(), Some("+00:00"));
        assert_eq!(log[0].stream.as_deref(), Some("stderr"));
        assert_eq!(log[0].thread.as_deref(), Some("tokio-runtime-worker"));
        assert_eq!(log[0].target.as_deref(), Some("slack::http"));
        assert_eq!(log[0].fields.as_deref(), Some(r#"{"retry_in":30}"#));
        assert_eq!(log[1].fields.as_deref(), Some(r#"{"hint":true}"#));
        assert_eq!(
            log[2].msg,
            "sealed checkpoint #1: 12 rows since the last, now at abc"
        );
        assert!(
            log.iter().all(|l| l.attempt == 2),
            "every line names the pass it belongs to"
        );
    }
}
