//! Publishing the event stream to the run store, one invocation at a time.
//!
//! An invocation's row is kept here and written whole: the store
//! coalesces to the newest row per invocation, so every row it sees has
//! to be complete. The `progress_length` / `progress_inc` sugar is also
//! resolved here — the wire carries deltas, the store carries positions,
//! and coalescing deltas would lose work.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, Weak};

use datalib_runs::store::{now_split, split_stamp};
use datalib_runs::{
    canonical_labels, InvocationMetricRow, InvocationRow, LiveState, LogRow, LoopWriter, Process,
    ProcessRow, Retention,
};

use crate::events::{Event, EventSink, LogLevel};
use crate::heavy_statements::{is_heavy_statement, HeavyStatements};
use crate::run_state::RunState;
use crate::slow_statements::{is_slow_statement, SlowStatements};
use crate::step::StepId;

/// What we know about one step: its latest invocation, and that
/// invocation's attempt.
#[derive(Default, Clone)]
struct Acc {
    /// The step's latest invocation; `None` before the first this sink
    /// saw. A value the runner keeps for the step after it ended (a
    /// consumer's queue) is filed under it.
    row: Option<InvocationRow>,
    /// The attempt's process: what came out of the step's pipes is its,
    /// and how it ended goes on it.
    process: Option<ProcessRow>,
    /// The sugar accumulators: increments so far, and the announced total.
    done: u64,
    total: Option<u64>,
    checkpoints: u64,
    /// The labels of every `queued` series written for the step, so its
    /// end can empty them all.
    queued: BTreeSet<String>,
    /// This attempt's last value of its own `queued`, the one without a
    /// `from`: what it still owes if it ends now.
    own_queued: Option<i64>,
    /// This attempt's last value of each running total (`name`, labels),
    /// and whether it has already been caught going down.
    counters: HashMap<(String, String), (i64, bool)>,
    /// What sqlx warned of, not yet summed up in a line.
    slow: SlowStatements,
    /// What `datalib_sql_work` warned of, likewise.
    heavy: HeavyStatements,
}

impl Acc {
    /// The invocation still running, for a line about it.
    fn live(&self) -> Option<&InvocationRow> {
        self.row
            .as_ref()
            .filter(|r| !datalib_runs::is_terminal(&r.state))
    }

    fn invocation_id(&self) -> String {
        self.row
            .as_ref()
            .map(|r| r.invocation_id.clone())
            .unwrap_or_default()
    }
}

/// The attempt's process row with its peak raised to `bytes`, to write
/// again; `None` when the row already says as much, or the attempt has
/// ended.
fn raise_peak(process: Option<&mut ProcessRow>, bytes: i64) -> Option<ProcessRow> {
    let p = process.filter(|p| p.finished_at_utc.is_none())?;
    if p.peak_rss_bytes.is_some_and(|peak| peak >= bytes) {
        return None;
    }
    p.peak_rss_bytes = Some(bytes);
    Some(p.clone())
}

/// What an invocation owes as it ends `status`, its own queue last at
/// `queued`. Only an end short of done owes: a render's or an index's bar
/// ends a successful invocation still holding work it announced and then
/// found it had no need to do, and a download that gives up with work
/// owed ends `incomplete`, which is a failure.
fn owed_at_end(status: RunState, queued: Option<i64>) -> i64 {
    match status {
        RunState::Failed | RunState::Stopped => queued.unwrap_or(0).max(0),
        RunState::Succeeded | RunState::SkippedUpToDate | RunState::Blocked | RunState::Running => {
            0
        }
    }
}

/// LoopState a running total's new value, and say what it was if this is
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
    writer: Arc<LoopWriter>,
    steps: Mutex<HashMap<StepId, Acc>>,
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

/// What a step's own queue held when its invocation ended; not emptied
/// with it.
const OWED: &str = datalib_metrics::OWED;

impl RunStoreSink {
    /// Returns `None` when the store could not be opened. The store is
    /// observability: a sync that runs without a record is much better
    /// than one that refuses to start because a status file was
    /// unwritable.
    pub fn start(
        data_root: &std::path::Path,
        git_hash: Option<String>,
        retention: Retention,
    ) -> Option<Self> {
        let writer = LoopWriter::start(data_root, git_hash, retention)?;
        Some(Self {
            writer: Arc::new(writer),
            steps: Mutex::new(HashMap::new()),
        })
    }

    /// For the runner's own `tracing` lines, through
    /// [`datalib_runs::StoreLayer`]: they land about no invocation.
    /// Weak, so the global subscriber never keeps the writer — and its
    /// final flush — from happening when this sink is dropped.
    pub fn log_sink(&self) -> Weak<LoopWriter> {
        Arc::downgrade(&self.writer)
    }

    /// Returns once everything sent before it is written.
    pub fn flush_blocking(&self) {
        self.writer.flush_blocking();
    }

    /// Change the step's latest invocation and write it whole; nothing
    /// for a step with none.
    fn update(&self, step: &StepId, f: impl FnOnce(&mut Acc)) {
        let row = {
            let mut steps = self.steps.lock().expect("run store sink mutex");
            let acc = steps.entry(step.clone()).or_default();
            f(acc);
            acc.row.as_mut().map(|row| {
                let (at, offset) = now();
                row.updated_at_utc = at;
                row.tz_offset = offset;
                row.clone()
            })
        };
        if let Some(row) = row {
            self.writer.invocation(row);
        }
    }

    fn metric(&self, step: &StepId, name: &str, labels: &BTreeMap<String, String>, value: i64) {
        let labels = canonical_labels(labels);
        let (fell_from, raised) = {
            let mut steps = self.steps.lock().expect("run store sink mutex");
            let acc = steps.entry(step.clone()).or_default();
            if name == QUEUED {
                acc.queued.insert(labels.clone());
                if labels.is_empty() {
                    acc.own_queued = Some(value);
                }
            }
            let raised = if name == crate::memory::RSS_PEAK_BYTES {
                raise_peak(acc.process.as_mut(), value)
            } else {
                None
            };
            let fell_from = if datalib_metrics::is_counter(name) {
                counter_fell(&mut acc.counters, name, &labels, value)
            } else {
                None
            };
            (fell_from, raised)
        };
        if let Some(before) = fell_from {
            self.warn_counter_fell(step, name, &labels, before, value);
        }
        // Written as it rises, not only at the end, so a runner that dies
        // mid-step still leaves the attempt's peak so far.
        if let Some(process) = raised {
            self.writer.process(process);
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
        self.say(
            step,
            None,
            LogLevel::Warn,
            format!(
                "{series} went down, {before} to {now}, but a name ending in `_total` says it only \
                 grows; name it without the suffix if it can fall (docs/dev/step_protocol.md)"
            ),
            serde_json::json!({ "counter_fell": series, "from": before, "to": now }),
        );
    }

    fn write_metric(&self, step: &StepId, name: &str, labels: String, value: i64) {
        let (updated_at_utc, tz_offset) = now();
        let invocation_id = self
            .steps
            .lock()
            .expect("run store sink mutex")
            .get(step)
            .map(Acc::invocation_id)
            .unwrap_or_default();
        self.writer.metric(InvocationMetricRow {
            invocation_id,
            step: step.clone(),
            name: name.to_string(),
            labels,
            value,
            updated_at_utc,
            tz_offset,
        });
    }

    /// The line's step, its group, and its invocation and attempt while
    /// that invocation runs: a line the runner writes about a step at rest
    /// is about no invocation.
    fn about(&self, step: &StepId) -> LogRow {
        let steps = self.steps.lock().expect("run store sink mutex");
        let acc = steps.get(step);
        let live = acc.and_then(Acc::live);
        LogRow {
            step: Some(step.clone()),
            group_id: acc
                .and_then(|a| a.row.as_ref())
                .and_then(|r| r.group_id.clone()),
            invocation_id: live.map(|r| r.invocation_id.clone()),
            attempt: live.map_or(0, |r| r.attempt),
            ..Default::default()
        }
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

    /// What the attempt's slow and heavy statements came to, once each,
    /// under the attempt and the process that ran them.
    fn say_slow_statements(&self, step: &StepId) {
        let (summaries, process_id) = {
            let mut steps = self.steps.lock().expect("run store sink mutex");
            let Some(acc) = steps.get_mut(step) else {
                return;
            };
            let process_id = acc.process.as_ref().map(|p| p.process_id.clone());
            let summaries = [
                std::mem::take(&mut acc.slow).summary(),
                std::mem::take(&mut acc.heavy).summary(),
            ];
            (summaries, process_id)
        };
        for (level, msg, fields) in summaries.into_iter().flatten() {
            self.say(step, process_id.clone(), level, msg, fields);
        }
    }

    /// A line the runner writes about a step.
    fn say(
        &self,
        step: &StepId,
        process_id: Option<String>,
        level: LogLevel,
        msg: String,
        fields: serde_json::Value,
    ) {
        let (ts_utc, tz_offset) = now();
        self.writer.log(LogRow {
            process_id: process_id.unwrap_or_default(),
            ts_utc,
            tz_offset,
            level: level.as_str().into(),
            target: Some(RUNNER_TARGET.into()),
            msg,
            fields: Some(fields.to_string()),
            ..self.about(step)
        });
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

    /// The invocation is over: its row and its last attempt's process
    /// closed, its queue emptied and what it owes kept, and the reason it
    /// ended badly a log line, so the log panel has a line to jump to —
    /// the last error-level line of a failed invocation is why it failed,
    /// the last warn-level one of a stopped invocation is where it
    /// stopped.
    #[allow(clippy::too_many_arguments)]
    fn ended(
        &self,
        step: &StepId,
        invocation: &str,
        status: RunState,
        error: &Option<String>,
        exit_code: Option<i32>,
        signal: Option<i32>,
        attempts: u32,
    ) {
        self.say_slow_statements(step);
        if let Some(error) = error {
            let level = if status == RunState::Stopped {
                LogLevel::Warn
            } else {
                LogLevel::Error
            };
            self.say(
                step,
                None,
                level,
                error.clone(),
                serde_json::json!({ "finished": status.as_str() }),
            );
        }
        let finished_at_utc = now().0;
        let mut process = None;
        let mut queued = BTreeSet::new();
        let mut owed = None;
        self.update(step, |a| {
            let Some(row) = a.row.as_mut().filter(|r| r.invocation_id == invocation) else {
                return;
            };
            row.state = status.as_str().into();
            row.attempt = i64::from(attempts.max(1));
            row.finished_at_utc = Some(finished_at_utc.clone());
            row.error = error.clone();
            row.exit_code = exit_code.map(i64::from);
            row.signal = signal.map(i64::from);
            queued = std::mem::take(&mut a.queued);
            owed = Some(owed_at_end(status, a.own_queued));
            if let Some(p) = a.process.as_mut().filter(|p| p.finished_at_utc.is_none()) {
                p.finished_at_utc = Some(finished_at_utc.clone());
                p.exit_code = exit_code.map(i64::from);
                p.signal = signal.map(i64::from);
                process = Some(p.clone());
            }
        });
        if let Some(p) = process {
            self.writer.process(p);
        }
        // Stays after the queue is emptied, until the next invocation ends.
        if let Some(owed) = owed {
            self.write_metric(step, OWED, String::new(), owed);
        }
        // Ended is ended, however: nothing is ahead of it now.
        for labels in queued {
            self.write_metric(step, QUEUED, labels, 0);
        }
    }
}

impl EventSink for RunStoreSink {
    fn emit(&self, event: &Event) {
        match event {
            Event::InvocationStart {
                step,
                invocation,
                group,
                verb,
                requests,
                now: step_now,
                fingerprint,
                reads,
            } => {
                let (started_at_utc, tz_offset) = now();
                let row = InvocationRow {
                    invocation_id: invocation.clone(),
                    step: step.clone(),
                    group_id: group.clone(),
                    verb: verb.map(|v| v.as_str().to_string()),
                    state: LiveState::Running.as_str().into(),
                    attempt: 0,
                    started_at_utc: started_at_utc.clone(),
                    now: Some(step_now.clone()),
                    fingerprint: Some(fingerprint.clone()),
                    reads: (!reads.is_empty())
                        .then(|| serde_json::to_string(reads).expect("a string map is JSON")),
                    updated_at_utc: started_at_utc,
                    tz_offset,
                    ..Default::default()
                };
                self.writer.served(invocation, requests);
                // What was ahead of it stays its to empty; the rest is new.
                self.update(step, |a| {
                    *a = Acc {
                        row: Some(row),
                        queued: std::mem::take(&mut a.queued),
                        ..Default::default()
                    }
                });
                self.say(
                    step,
                    None,
                    LogLevel::Info,
                    match verb {
                        Some(verb) => {
                            format!("started with {} for {}", verb.flag(), requests.join(", "))
                        }
                        None => format!("started for {}", requests.join(", ")),
                    },
                    serde_json::json!({ "requests": requests, "now": step_now }),
                );
            }
            // A retry re-runs the step from zero, so the counters reset
            // with it — otherwise attempt 2 would appear to start
            // wherever attempt 1 died.
            Event::StepStart { step, attempt } => {
                // A retry goes straight to its next start.
                self.say_slow_statements(step);
                let (started_at_utc, tz_offset) = now();
                let mut process = None;
                self.update(step, |a| {
                    let Some(row) = a.row.as_mut() else { return };
                    row.attempt = i64::from(*attempt);
                    let p = ProcessRow {
                        process_id: datalib_runs::new_process_id(),
                        process: Process::Step.as_str().into(),
                        invocation_id: Some(row.invocation_id.clone()),
                        step: Some(step.clone()),
                        attempt: Some(i64::from(*attempt)),
                        started_at_utc: started_at_utc.clone(),
                        tz_offset: tz_offset.clone(),
                        ..Default::default()
                    };
                    process = Some(p.clone());
                    *a = Acc {
                        row: a.row.take(),
                        queued: std::mem::take(&mut a.queued),
                        process: Some(p),
                        ..Default::default()
                    };
                });
                if let Some(p) = process {
                    self.writer.process(p);
                }
            }
            Event::InvocationEnd {
                step,
                invocation,
                status,
                error,
                exit_code,
                signal,
                attempts,
            } => self.ended(
                step, invocation, *status, error, *exit_code, *signal, *attempts,
            ),
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
            Event::ProgressMessage { step, msg } => self.update(step, |a| {
                if let Some(row) = a.row.as_mut() {
                    row.msg = Some(msg.clone());
                }
            }),
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
                let since_last = match rows {
                    Some(rows) => format!("{rows} rows since the last, "),
                    None => String::new(),
                };
                self.say(
                    step,
                    None,
                    LogLevel::Info,
                    format!("sealed checkpoint #{n}: {since_last}now at {version}"),
                    serde_json::json!({ "version": version, "rows": rows, "checkpoint": n }),
                );
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
                let level = if is_slow_statement(*level, target.as_deref()) {
                    self.steps
                        .lock()
                        .expect("run store sink mutex")
                        .entry(step.clone())
                        .or_default()
                        .slow
                        .note(fields.as_ref());
                    LogLevel::Debug
                } else if is_heavy_statement(*level, target.as_deref()) {
                    self.steps
                        .lock()
                        .expect("run store sink mutex")
                        .entry(step.clone())
                        .or_default()
                        .heavy
                        .note(fields.as_ref());
                    LogLevel::Debug
                } else {
                    *level
                };
                self.writer.log(LogRow {
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
                    ..self.about(step)
                })
            }
            Event::Hint { step, msg } => self.say(
                step,
                None,
                LogLevel::Warn,
                msg.clone(),
                serde_json::json!({ "hint": true }),
            ),
            // The commit a step says its program came from goes on its
            // attempt's process row.
            Event::Capabilities {
                step,
                version: Some(version),
                ..
            } => {
                let mut row = None;
                self.update(step, |a| {
                    if let Some(p) = a.process.as_mut() {
                        p.git_hash = Some(version.clone());
                        row = Some(p.clone());
                    }
                });
                if let Some(row) = row {
                    self.writer.process(row);
                }
            }
            // Nothing else to record: whether a sink streams is about what
            // the step *can* do; the plan is about the loop taking work on,
            // which the store does not keep, and a request's end is in
            // `system/supervisor.sqlite`.
            Event::Capabilities { .. } | Event::RunPlan { .. } | Event::RequestEnd { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_runs::{
        log_query, processes, snapshot, LogCursor, LogLine, LogQuery, Snapshot, Stream,
    };

    async fn run(events: &[Event]) -> (tempfile::TempDir, Snapshot) {
        let td = tempfile::tempdir().unwrap();
        record(td.path(), events);
        let snap = snapshot(td.path()).await.unwrap();
        (td, snap)
    }

    /// The events, through a sink dropped at the end, which flushes and
    /// joins its writer.
    fn record(root: &std::path::Path, events: &[Event]) {
        let sink = RunStoreSink::start(root, Some("ae2d52f0".into()), Retention::default())
            .expect("start the store");
        for e in events {
            sink.emit(e);
        }
    }

    /// Every line the store holds, oldest first; with `q`, those it keeps.
    async fn lines(root: &std::path::Path, q: &str) -> Vec<LogLine> {
        log_query(
            root,
            &LogQuery {
                q,
                cursor: LogCursor::After(0),
                limit: 100,
            },
        )
        .await
        .unwrap()
    }

    /// The lines the runner did not write to say an invocation started.
    fn but_starts(lines: Vec<LogLine>) -> Vec<LogLine> {
        lines
            .into_iter()
            .filter(|l| !l.msg.starts_with("started "))
            .collect()
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

    /// The loop starting an invocation of `step` for request `r-1`, and
    /// its first attempt.
    fn begin(step: &str, invocation: &str) -> [Event; 2] {
        [
            Event::InvocationStart {
                step: step.into(),
                invocation: invocation.into(),
                group: step.split_once('/').map(|(g, _)| g.to_string()),
                verb: None,
                requests: vec!["r-1".into()],
                now: "2026-09-11T10:00:00+01:00".into(),
                fingerprint: "fp".into(),
                reads: BTreeMap::new(),
            },
            attempt(step, 1),
        ]
    }

    fn attempt(step: &str, attempt: u32) -> Event {
        Event::StepStart {
            step: step.into(),
            attempt,
        }
    }

    fn end(step: &str, invocation: &str, status: RunState, error: Option<&str>) -> Event {
        Event::InvocationEnd {
            step: step.into(),
            invocation: invocation.into(),
            status,
            error: error.map(str::to_string),
            exit_code: Some(if status == RunState::Succeeded { 0 } else { 1 }),
            signal: None,
            attempts: 1,
        }
    }

    /// Guards the render that wrote 21 `warn` lines and counted them as
    /// the step's errors: each stays, at `debug`, and the attempt's end
    /// says what they came to, under the attempt that ran them.
    #[tokio::test]
    async fn slow_statements_are_kept_quiet_and_summed_at_the_attempts_end() {
        let step = "slack/render_markdown";
        let slow = |secs: f64| Event::Log {
            step: step.into(),
            level: LogLevel::Warn,
            msg: "slow statement: execution time exceeded alert threshold".into(),
            ts: None,
            stream: Some(Stream::Stderr),
            target: Some("sqlx::query".into()),
            thread: None,
            fields: serde_json::json!({
                "db.statement": "SELECT id FROM records",
                "elapsed_secs": secs,
            })
            .as_object()
            .cloned(),
        };
        let mut events = begin(step, "i-1").to_vec();
        events.extend([
            slow(2.0),
            slow(3.0),
            end(step, "i-1", RunState::Succeeded, None),
        ]);
        let (td, snap) = run(&events).await;
        let lines = but_starts(lines(td.path(), "").await);
        let levels: Vec<&str> = lines.iter().map(|l| l.level.as_str()).collect();
        assert_eq!(levels, ["debug", "debug", "info"], "{lines:#?}");
        assert_eq!(
            lines[2].msg,
            "2 slow statements took 5.0s in all, every one: SELECT id FROM records"
        );
        assert_eq!(lines[2].step.as_deref(), Some(step));
        assert_eq!(lines[2].invocation_id.as_deref(), Some("i-1"));
        assert_eq!(lines[2].attempt, 1);
        assert_eq!(lines[2].process.as_deref(), Some("step"));
        assert_eq!(snap.errors.get(step), None);
    }

    /// A heavy statement's line stays, at `debug`, and the attempt's end
    /// names the statement that did the work and the file it read.
    #[tokio::test]
    async fn heavy_statements_are_kept_quiet_and_summed_at_the_attempts_end() {
        let step = "unified_grid_and_search_index/unified_grid_and_search_index";
        let heavy = |vm_steps: u64| Event::Log {
            step: step.into(),
            level: LogLevel::Warn,
            msg: format!("heavy statement: {vm_steps} steps for 0 rows"),
            ts: None,
            stream: Some(Stream::Stderr),
            target: Some(datalib_sql_work::TARGET.into()),
            thread: None,
            fields: serde_json::json!({
                "statement": "SELECT n.uuid FROM data_nodes n WHERE n.source_id = ?",
                "file": "/root/unified_grid_and_search_index/db.sqlite",
                "params": "text(12)",
                "vm_steps": vm_steps,
                "rows": 0,
            })
            .as_object()
            .cloned(),
        };
        let mut events = begin(step, "i-1").to_vec();
        events.extend([
            heavy(25_000_000),
            heavy(30_000_000),
            end(step, "i-1", RunState::Succeeded, None),
        ]);
        let (td, snap) = run(&events).await;
        let lines = but_starts(lines(td.path(), "").await);
        let levels: Vec<&str> = lines.iter().map(|l| l.level.as_str()).collect();
        assert_eq!(levels, ["debug", "debug", "info"], "{lines:#?}");
        assert_eq!(
            lines[2].msg,
            "2 heavy statements did 55.0M steps for 0 rows, every one on db.sqlite: \
             SELECT n.uuid FROM data_nodes n WHERE n.source_id = ?"
        );
        assert_eq!(lines[2].attempt, 1);
        assert_eq!(snap.errors.get(step), None);
    }

    /// The runner is a process of its own, outside every invocation, and
    /// a line it writes about a step at rest is about no invocation.
    #[tokio::test]
    async fn the_runners_lines_name_its_process_and_no_invocation() {
        let (td, _snap) = run(&[Event::Log {
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
            .unwrap()
            .into_iter()
            .find(|p| p.process == "dag")
            .expect("the runner is a process");
        assert_eq!(runner.run_id, None);
        assert_eq!(runner.git_hash.as_deref(), Some("ae2d52f0"));
        let lines = lines(td.path(), "").await;
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].process_id, runner.process_id);
        assert_eq!(lines[0].invocation_id, None);
        assert_eq!(lines[0].process.as_deref(), Some("dag"));
        assert_eq!(lines[0].git_hash.as_deref(), Some("ae2d52f0"));
    }

    /// An invocation names the requests it served, and the store answers
    /// "what did this request run" and "this request's log" from that.
    #[tokio::test]
    async fn an_invocation_is_filed_under_the_requests_it_served() {
        let td = tempfile::tempdir().unwrap();
        let start = |step: &str, invocation: &str, requests: &[&str]| Event::InvocationStart {
            step: step.into(),
            invocation: invocation.into(),
            group: Some("slack".into()),
            verb: None,
            requests: requests.iter().map(|r| r.to_string()).collect(),
            now: "2026-09-11T10:00:00+01:00".into(),
            fingerprint: "fp-1".into(),
            reads: BTreeMap::from([("slack/ingest".into(), "fp-0:v1".into())]),
        };
        let said = |step: &str| Event::Log {
            step: step.into(),
            level: LogLevel::Info,
            msg: format!("{step} here"),
            ts: None,
            stream: Some(Stream::Stderr),
            target: None,
            thread: None,
            fields: None,
        };
        record(
            td.path(),
            &[
                start("slack/ingest", "i-1", &["r-a"]),
                attempt("slack/ingest", 1),
                said("slack/ingest"),
                end("slack/ingest", "i-1", RunState::Succeeded, None),
                start("slack/render_markdown", "i-2", &["r-a", "r-b"]),
                attempt("slack/render_markdown", 1),
                said("slack/render_markdown"),
                end("slack/render_markdown", "i-2", RunState::Succeeded, None),
            ],
        );
        let of = |r: &'static str| {
            let root = td.path().to_path_buf();
            async move {
                datalib_runs::invocations_of(&root, r)
                    .await
                    .unwrap()
                    .into_iter()
                    .map(|i| i.invocation_id)
                    .collect::<Vec<_>>()
            }
        };
        assert_eq!(of("r-a").await, ["i-1", "i-2"]);
        assert_eq!(of("r-b").await, ["i-2"]);
        let render = &datalib_runs::invocations_of(td.path(), "r-b")
            .await
            .unwrap()[0];
        assert_eq!(render.fingerprint.as_deref(), Some("fp-1"));
        assert_eq!(
            render.reads.as_deref(),
            Some(r#"{"slack/ingest":"fp-0:v1"}"#)
        );
        assert_eq!(render.now.as_deref(), Some("2026-09-11T10:00:00+01:00"));
        assert_eq!(render.state, "succeeded");

        let msgs = |lines: Vec<LogLine>| -> Vec<String> {
            but_starts(lines).into_iter().map(|l| l.row.msg).collect()
        };
        assert_eq!(
            msgs(lines(td.path(), "run:r-b").await),
            ["slack/render_markdown here"]
        );
        assert_eq!(
            msgs(lines(td.path(), "run:r-a").await),
            ["slack/ingest here", "slack/render_markdown here"]
        );
        assert_eq!(
            msgs(lines(td.path(), "invocation:i-1").await),
            ["slack/ingest here"]
        );
        let requests = datalib_runs::requests_of_steps(
            td.path(),
            Some(&["slack/render_markdown".to_string()]),
            10,
        )
        .await
        .unwrap();
        let mut ids: Vec<String> = requests.into_iter().map(|(id, _)| id).collect();
        ids.sort();
        assert_eq!(ids, ["r-a", "r-b"]);
    }

    /// Every line about a step carries its group — its own output, and
    /// what the runner said about it — so `group:` finds a source's lines
    /// whatever step wrote them.
    #[tokio::test]
    async fn a_steps_lines_carry_its_group() {
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
        let mut events = begin("slack/ingest", "i-1").to_vec();
        events.extend([
            log("slack/ingest", Some(Stream::Stderr)),
            end("slack/ingest", "i-1", RunState::Failed, Some("boom")),
        ]);
        events.extend(begin("slack/render_markdown", "i-2"));
        events.push(log("slack/render_markdown", None));
        let (td, _snap) = run(&events).await;
        let lines = but_starts(lines(td.path(), "").await);
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
            ]
        );
        assert_eq!(
            but_starts(self::lines(td.path(), "group:slack").await).len(),
            3
        );
    }

    /// A streaming consumer is invoked once per producer checkpoint, and
    /// every invocation is a process that ends with an exit code while the
    /// next one runs.
    #[tokio::test]
    async fn each_streaming_invocation_closes_its_process() {
        let step = "unified_grid_and_search_index/grid";
        let mut events = begin(step, "i-1").to_vec();
        events.push(end(step, "i-1", RunState::Succeeded, None));
        events.extend(begin(step, "i-2"));
        let (td, snap) = run(&events).await;
        let mut passes: Vec<_> = processes(td.path(), None, Some("step"), 10)
            .await
            .unwrap()
            .into_iter()
            .filter(|p| p.step.as_deref() == Some(step))
            .collect();
        passes.sort_by(|a, b| a.started_at_utc.cmp(&b.started_at_utc));
        assert_eq!(passes.len(), 2, "one process per invocation");
        assert_eq!(passes[0].invocation_id.as_deref(), Some("i-1"));
        assert_eq!(passes[0].exit_code, Some(0));
        assert!(passes[0].finished_at_utc.is_some(), "the first ended");
        assert!(passes[1].finished_at_utc.is_none(), "the second is running");
        let newest = snap.steps.iter().find(|s| s.step == step).unwrap();
        assert_eq!(newest.invocation_id.as_deref(), Some("i-2"));
        assert_eq!(newest.state, "running");
    }

    /// An invocation that failed and was followed by one that succeeded
    /// keeps its failure: its row says so, and its error is its last line.
    #[tokio::test]
    async fn a_failed_invocation_stays_failed_though_the_next_one_succeeds() {
        let step = "evals/score";
        let mut events = begin(step, "i-1").to_vec();
        events.push(end(
            step,
            "i-1",
            RunState::Failed,
            Some("step evals/score exited with exit status: 1: Traceback"),
        ));
        events.extend(begin(step, "i-2"));
        events.push(end(step, "i-2", RunState::Succeeded, None));
        let (td, snap) = run(&events).await;
        assert_eq!(snap.steps[0].state, "succeeded");
        let first = &datalib_runs::invocations_of(td.path(), "r-1")
            .await
            .unwrap()[0];
        assert_eq!(first.state, "failed");
        let errors: Vec<(String, Option<String>)> = lines(td.path(), "level:error")
            .await
            .into_iter()
            .map(|l| (l.row.msg, l.row.invocation_id))
            .collect();
        assert_eq!(
            errors,
            [(
                "step evals/score exited with exit status: 1: Traceback".to_string(),
                Some("i-1".to_string())
            )]
        );
    }

    /// The most memory a step's attempt held goes on that attempt's
    /// process row: a retry's peak is its own, and a fall after the peak
    /// does not lower it.
    #[tokio::test]
    async fn a_step_attempts_peak_memory_goes_on_its_process() {
        let peak = |value| Event::Metric {
            step: "slack/raw".into(),
            name: crate::memory::RSS_PEAK_BYTES.into(),
            labels: BTreeMap::new(),
            value,
        };
        let mut events = begin("slack/raw", "i-1").to_vec();
        events.extend([
            peak(1000),
            peak(5000),
            peak(3000),
            attempt("slack/raw", 2),
            peak(2000),
            end("slack/raw", "i-1", RunState::Succeeded, None),
        ]);
        let (td, _snap) = run(&events).await;
        let mut attempts: Vec<_> = processes(td.path(), None, None, 10)
            .await
            .unwrap()
            .into_iter()
            .filter(|p| p.process == "step")
            .collect();
        attempts.sort_by_key(|p| p.attempt);
        let peaks: Vec<_> = attempts.iter().map(|p| p.peak_rss_bytes).collect();
        assert_eq!(peaks, [Some(5000), Some(2000)], "{attempts:?}");
    }

    /// Each attempt of an invocation is a process: what came out of its
    /// pipes is its, the commit its `capabilities` event names is its (and
    /// never the runner's), and how the last one ended goes on it.
    #[tokio::test]
    async fn an_invocations_attempt_is_a_process_with_its_lines_and_its_exit() {
        let mut events = begin("slack/raw", "i-1").to_vec();
        events.extend([
            Event::Capabilities {
                step: "slack/raw".into(),
                streams_output: false,
                version: Some("c0ffee12".into()),
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
            attempt("slack/raw", 2),
            Event::InvocationEnd {
                step: "slack/raw".into(),
                invocation: "i-1".into(),
                status: RunState::Failed,
                error: Some("exited 3".into()),
                exit_code: Some(3),
                signal: None,
                attempts: 2,
            },
        ]);
        let (td, snap) = run(&events).await;
        let mut attempts: Vec<_> = processes(td.path(), None, None, 10)
            .await
            .unwrap()
            .into_iter()
            .filter(|p| p.process == "step")
            .collect();
        attempts.sort_by_key(|p| p.attempt);
        assert_eq!(attempts.len(), 2, "{attempts:?}");
        assert_eq!(attempts[0].step.as_deref(), Some("slack/raw"));
        assert_eq!(attempts[0].invocation_id.as_deref(), Some("i-1"));
        assert_eq!(attempts[0].run_id, None);
        assert_eq!(attempts[0].git_hash.as_deref(), Some("c0ffee12"));
        assert_eq!(
            attempts[1].git_hash, None,
            "an attempt that names no commit is not given the runner's"
        );
        assert_eq!(attempts[1].exit_code, Some(3));
        assert_eq!(snap.steps[0].attempt, 2);
        let lines = lines(td.path(), "").await;
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
        assert_ne!(reason.process_id, attempts[1].process_id);
        assert_eq!(reason.process.as_deref(), Some("dag"));
        assert_eq!(reason.invocation_id.as_deref(), Some("i-1"));
    }

    #[tokio::test]
    async fn increments_accumulate_into_done_and_queued() {
        let mut events = begin("slack/raw", "i-1").to_vec();
        events.extend([
            Event::ProgressLength {
                step: "slack/raw".into(),
                total: Some(9),
            },
            inc("slack/raw", 1),
            inc("slack/raw", 3),
            inc("slack/raw", 2),
        ]);
        let (_td, snap) = run(&events).await;

        assert_eq!(snap.steps.len(), 1);
        assert_eq!(snap.steps[0].state, LiveState::Running.as_str());
        assert_eq!(
            metric_value(&snap, "slack/raw", datalib_metrics::DONE),
            Some(6),
            "1 + 3 + 2, not the last delta"
        );
        assert_eq!(metric_value(&snap, "slack/raw", "queued"), Some(3));
    }

    /// An invocation that has ended has nothing ahead of it, however it
    /// ended: its own `queued` and every `queued{from=…}` go to zero, the
    /// one published before it started included.
    #[tokio::test]
    async fn every_ending_empties_the_queue() {
        for status in [RunState::Succeeded, RunState::Failed, RunState::Stopped] {
            let mut events = vec![Event::Metric {
                step: "slack/raw".into(),
                name: "queued".into(),
                labels: BTreeMap::from([("from".to_string(), "slack/list".to_string())]),
                value: 5,
            }];
            events.extend(begin("slack/raw", "i-1"));
            events.extend([
                Event::ProgressLength {
                    step: "slack/raw".into(),
                    total: Some(9),
                },
                inc("slack/raw", 3),
                end("slack/raw", "i-1", status, None),
            ]);
            let (_td, snap) = run(&events).await;
            let queued: BTreeMap<String, i64> = snap
                .metrics
                .iter()
                .filter(|m| m.name == "queued" && m.reported_in.id() == "i-1")
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

    /// Emptying a stopped step's queue left nothing to say what it still
    /// owed: Slack's conversations, threads and files not reached were
    /// only in its store. What its own queue held as its invocation ended
    /// is its `owed`, which outlives the invocation; a producer's queue
    /// for it is not owed, and the step's next invocation to end replaces
    /// the count.
    #[tokio::test]
    async fn what_a_step_owes_when_it_ends_outlives_it_until_its_next_ends() {
        let td = tempfile::tempdir().unwrap();
        let length = |total| Event::ProgressLength {
            step: "slack/ingest".into(),
            total: Some(total),
        };
        let from_ingest = Event::Metric {
            step: "slack/render_markdown".into(),
            name: QUEUED.into(),
            labels: BTreeMap::from([("from".to_string(), "slack/ingest".to_string())]),
            value: 40,
        };
        async fn owed(root: &std::path::Path) -> Vec<(String, i64, String)> {
            datalib_runs::latest_metric(root, OWED)
                .await
                .unwrap()
                .into_iter()
                .map(|m| (m.step, m.value, m.reported_in.id().to_string()))
                .collect()
        }

        let mut first = begin("slack/ingest", "i-1").to_vec();
        first.extend([
            length(12),
            inc("slack/ingest", 3),
            end("slack/ingest", "i-1", RunState::Stopped, None),
        ]);
        first.extend(begin("slack/render_markdown", "i-2"));
        first.extend([
            from_ingest,
            end("slack/render_markdown", "i-2", RunState::Stopped, None),
        ]);
        record(td.path(), &first);
        let snap = snapshot(td.path()).await.unwrap();
        assert_eq!(
            metric_value(&snap, "slack/ingest", QUEUED),
            Some(0),
            "the queue is emptied as before"
        );
        assert_eq!(
            owed(td.path()).await,
            [
                ("slack/ingest".to_string(), 9, "i-1".to_string()),
                ("slack/render_markdown".to_string(), 0, "i-2".to_string()),
            ]
        );

        let mut second = begin("slack/ingest", "i-3").to_vec();
        second.extend([
            length(4),
            inc("slack/ingest", 4),
            end("slack/ingest", "i-3", RunState::Succeeded, None),
        ]);
        record(td.path(), &second);
        assert_eq!(
            owed(td.path()).await,
            [
                ("slack/ingest".to_string(), 0, "i-3".to_string()),
                ("slack/render_markdown".to_string(), 0, "i-2".to_string()),
            ],
            "a step not invoked again keeps what its last invocation said"
        );
    }

    /// Renders and embeds end most successful runs with work still on
    /// their bar that they found they had no need to do, which read as
    /// "20,349 owed" on a healthy source. Only an end short of done owes.
    #[test]
    fn only_an_invocation_that_ended_short_of_done_owes() {
        use strum::VariantArray;
        let ends: Vec<(RunState, i64)> = RunState::VARIANTS
            .iter()
            .map(|&s| (s, owed_at_end(s, Some(20_349))))
            .collect();
        assert_eq!(
            ends,
            [
                (RunState::Running, 0),
                (RunState::Succeeded, 0),
                (RunState::SkippedUpToDate, 0),
                (RunState::Blocked, 0),
                (RunState::Failed, 20_349),
                (RunState::Stopped, 20_349),
            ]
        );
        assert_eq!(owed_at_end(RunState::Failed, None), 0);
        assert_eq!(owed_at_end(RunState::Stopped, Some(-3)), 0);
    }

    /// A retry re-runs the step from the beginning, so the count has to
    /// go back to zero. Otherwise attempt 2 appears to resume from
    /// wherever attempt 1 died and can sail past `total`.
    #[tokio::test]
    async fn a_retry_restarts_the_count() {
        let mut events = begin("slack/raw", "i-1").to_vec();
        events.extend([
            Event::ProgressLength {
                step: "slack/raw".into(),
                total: Some(9),
            },
            inc("slack/raw", 7),
            attempt("slack/raw", 2),
            inc("slack/raw", 1),
        ]);
        let (_td, snap) = run(&events).await;

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
        let mut events = begin("slack/raw", "i-1").to_vec();
        events.extend([
            table("messages", 10),
            table("channels", 2),
            table("messages", 25),
        ]);
        let (_td, snap) = run(&events).await;
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
        let mut events = begin("slack/raw", "i-1").to_vec();
        events.extend([
            m("api_requests_total", 5),
            m("api_requests_total", 3),
            m("api_requests_total", 2),
            m("queued", 9),
            m("queued", 1),
        ]);
        let (td, snap) = run(&events).await;
        assert_eq!(
            metric_value(&snap, "slack/raw", "api_requests_total"),
            Some(2)
        );
        let warned: Vec<String> = lines(td.path(), "step:slack/raw level:warn")
            .await
            .into_iter()
            .map(|l| l.row.msg)
            .collect();
        assert_eq!(warned.len(), 1, "{warned:?}");
        assert!(
            warned[0].starts_with("api_requests_total went down, 5 to 3"),
            "{warned:?}"
        );
    }

    /// The step's own words reach the store, and the invocation's end
    /// sticks with its error and its finish time. The error is also the
    /// last line of the step's log, at error level, so the log panel
    /// has a line to jump to; a stop is a warning there, not an error.
    #[tokio::test]
    async fn the_last_message_and_the_outcome_are_recorded() {
        let mut events = begin("slack/raw", "i-1").to_vec();
        events.extend([
            Event::ProgressMessage {
                step: "slack/raw".into(),
                msg: "conversations.list".into(),
            },
            end("slack/raw", "i-1", RunState::Failed, Some("boom")),
            // A late update of a finished invocation does not reopen it.
            Event::ProgressMessage {
                step: "slack/raw".into(),
                msg: "late".into(),
            },
        ]);
        let (td, snap) = run(&events).await;

        let s = &snap.steps[0];
        assert_eq!(s.state, RunState::Failed.as_str());
        assert_eq!(s.error.as_deref(), Some("boom"));
        assert!(s.started_at_utc.is_some() && s.finished_at_utc.is_some());
        let log = but_starts(lines(td.path(), "step:slack/raw").await);
        assert_eq!(log.len(), 1, "{log:?}");
        assert_eq!(
            (log[0].level.as_str(), log[0].msg.as_str()),
            ("error", "boom")
        );
        assert_eq!(log[0].fields.as_deref(), Some(r#"{"finished":"failed"}"#));

        let mut events = begin("slack/raw", "i-1").to_vec();
        events.push(end(
            "slack/raw",
            "i-1",
            RunState::Stopped,
            Some("stopped when asked to"),
        ));
        events.extend(begin("slack/rendered_md", "i-2"));
        events.push(end("slack/rendered_md", "i-2", RunState::Succeeded, None));
        let (td, _) = run(&events).await;
        let log = but_starts(lines(td.path(), "").await);
        assert_eq!(log.len(), 1, "a clean end logs nothing: {log:?}");
        assert_eq!(log[0].level, "warn");
    }

    /// Log lines, hints and checkpoints all land in the log, unwrapped,
    /// and none of them disturbs the invocation's message.
    #[tokio::test]
    async fn logs_hints_and_checkpoints_land_in_the_log() {
        let mut events = begin("slack/raw", "i-1").to_vec();
        events.extend([
            attempt("slack/raw", 2),
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
        ]);
        let (td, snap) = run(&events).await;

        assert_eq!(snap.steps[0].msg.as_deref(), Some("conversations.list"));
        assert_eq!(
            metric_value(&snap, "slack/raw", datalib_metrics::CHECKPOINTS),
            Some(1)
        );
        let log = but_starts(lines(td.path(), "step:slack/raw").await);
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
            log.iter()
                .all(|l| l.attempt == 2 && l.invocation_id.as_deref() == Some("i-1")),
            "every line names the attempt and the invocation it belongs to"
        );
    }
}
