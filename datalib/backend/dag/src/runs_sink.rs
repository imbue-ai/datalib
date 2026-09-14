//! Publishing the event stream to the run store.
//!
//! Step state is kept here and written whole: the store coalesces to the
//! newest row per step, so every row it sees has to be complete. The
//! `progress_length` / `progress_inc` sugar is also resolved here — the
//! wire carries deltas, the store carries positions, and coalescing
//! deltas would lose work.

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;

use datalib_runs::{canonical_labels, LiveState, LogRow, MetricRow, Retention, RunWriter, StepRow};

use crate::events::{Event, EventSink};
use crate::step::StepId;

/// What we know about one step right now.
#[derive(Default, Clone)]
struct Acc {
    row: StepRow,
    /// The sugar accumulators: increments so far, and the announced total.
    done: u64,
    total: Option<u64>,
    checkpoints: u64,
}

/// An [`EventSink`] that keeps the run store current.
pub struct RunStoreSink {
    writer: RunWriter,
    steps: Mutex<HashMap<StepId, Acc>>,
}

fn now() -> String {
    datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339()
}

impl RunStoreSink {
    /// Returns `None` when the store could not be opened. The store is
    /// observability: a sync that runs without a record is much better
    /// than one that refuses to start because a status file was
    /// unwritable.
    pub fn start(
        data_root: &std::path::Path,
        run_id: &str,
        started_at: &str,
        retention: Retention,
    ) -> Option<Self> {
        Some(Self {
            writer: RunWriter::start(data_root, run_id, started_at, retention)?,
            steps: Mutex::new(HashMap::new()),
        })
    }

    fn update(&self, step: &StepId, f: impl FnOnce(&mut Acc)) {
        let row = {
            let mut steps = self.steps.lock().expect("run store sink mutex");
            let acc = steps.entry(step.clone()).or_default();
            f(acc);
            acc.row.step = step.clone();
            acc.row.updated_at = now();
            acc.row.clone()
        };
        self.writer.step(row);
    }

    fn metric(&self, step: &StepId, name: &str, labels: &BTreeMap<String, String>, value: i64) {
        self.writer.metric(MetricRow {
            step: step.clone(),
            name: name.to_string(),
            labels: canonical_labels(labels),
            value,
            updated_at: now(),
        });
    }

    fn attempt_of(&self, step: &StepId) -> u32 {
        self.steps
            .lock()
            .expect("run store sink mutex")
            .get(step)
            .map(|a| a.row.attempt)
            .unwrap_or(0)
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
        self.metric(step, "done", &none, done as i64);
        if let Some(total) = total {
            self.metric(step, "queued", &none, total.saturating_sub(done) as i64);
        }
    }
}

impl EventSink for RunStoreSink {
    fn emit(&self, event: &Event) {
        match event {
            // Publish the whole plan up front so a reader can draw every
            // row, pending ones included, before anything has started.
            Event::RunPlan { steps } => {
                for step in steps {
                    self.update(step, |a| a.row.state = LiveState::Pending.as_str().into());
                }
            }
            // A retry re-runs the step from zero, so the counters reset
            // with it — otherwise attempt 2 would appear to start
            // wherever attempt 1 died.
            Event::StepStart { step, attempt } => self.update(step, |a| {
                *a = Acc {
                    row: StepRow {
                        state: LiveState::Running.as_str().into(),
                        attempt: *attempt,
                        started_at: Some(now()),
                        ..Default::default()
                    },
                    ..Default::default()
                }
            }),
            Event::StepFinish {
                step,
                status,
                error,
            } => self.update(step, |a| {
                a.row.state = status.as_str().into();
                a.row.finished_at = Some(now());
                a.row.error = error.clone();
            }),
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
            Event::Checkpoint { step, version } => {
                let n = {
                    let mut steps = self.steps.lock().expect("run store sink mutex");
                    let acc = steps.entry(step.clone()).or_default();
                    acc.checkpoints += 1;
                    acc.checkpoints
                };
                self.metric(step, "checkpoints", &BTreeMap::new(), n as i64);
                self.writer.log(LogRow {
                    step: Some(step.clone()),
                    attempt: self.attempt_of(step),
                    ts: now(),
                    level: "info".into(),
                    msg: "sealed a checkpoint".into(),
                    fields: Some(serde_json::json!({ "version": version }).to_string()),
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
            } => self.writer.log(LogRow {
                step: Some(step.clone()),
                attempt: self.attempt_of(step),
                // The line's own clock when it has one — that is when the
                // step wrote it, where ours is when we read it.
                ts: ts.clone().unwrap_or_else(now),
                stream: stream.map(|s| s.as_str().to_string()),
                level: level.as_str().into(),
                target: target.clone(),
                thread: thread.clone(),
                msg: msg.clone(),
                fields: fields
                    .as_ref()
                    .map(|f| serde_json::Value::Object(f.clone()).to_string()),
                ..Default::default()
            }),
            Event::Hint { step, msg } => self.writer.log(LogRow {
                step: Some(step.clone()),
                attempt: self.attempt_of(step),
                ts: now(),
                level: "warn".into(),
                msg: msg.clone(),
                fields: Some(r#"{"hint":true}"#.into()),
                ..Default::default()
            }),
            // Nothing to record: a capability is about what the step
            // *can* do, and the summary repeats what `StepFinish` said.
            Event::Capabilities { .. } | Event::RunSummary { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_state::RunState;
    use datalib_runs::{log_after, snapshot, Snapshot};

    async fn run(events: &[Event]) -> (tempfile::TempDir, Snapshot) {
        let td = tempfile::tempdir().unwrap();
        {
            let sink = RunStoreSink::start(
                td.path(),
                "run-1",
                "2026-09-11T10:00:00+01:00",
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
    #[tokio::test]
    async fn increments_accumulate_into_done_and_queued() {
        let (_td, snap) = run(&[
            Event::StepStart {
                step: "slack/raw".into(),
                attempt: 1,
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
            metric_value(&snap, "slack/raw", "done"),
            Some(6),
            "1 + 3 + 2, not the last delta"
        );
        assert_eq!(metric_value(&snap, "slack/raw", "queued"), Some(3));
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
            },
            Event::ProgressLength {
                step: "slack/raw".into(),
                total: Some(9),
            },
            inc("slack/raw", 7),
            Event::StepStart {
                step: "slack/raw".into(),
                attempt: 2,
            },
            inc("slack/raw", 1),
        ])
        .await;

        assert_eq!(
            metric_value(&snap, "slack/raw", "done"),
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
            name: "rows_upserted".into(),
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

    /// A reader must be able to draw the whole table before the first
    /// step starts, which is what makes the plan event worth publishing.
    #[tokio::test]
    async fn the_plan_lands_before_anything_runs() {
        let (_td, snap) = run(&[Event::RunPlan {
            steps: vec!["slack/raw".into(), "slack/rendered_md".into()],
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
    /// sticks with its error and its finish time.
    #[tokio::test]
    async fn the_last_message_and_the_outcome_are_recorded() {
        let (_td, snap) = run(&[
            Event::StepStart {
                step: "slack/raw".into(),
                attempt: 1,
            },
            Event::ProgressMessage {
                step: "slack/raw".into(),
                msg: "conversations.list".into(),
            },
            Event::StepFinish {
                step: "slack/raw".into(),
                status: RunState::Failed,
                error: Some("boom".into()),
            },
        ])
        .await;

        let s = &snap.steps[0];
        assert_eq!(s.state, RunState::Failed.as_str());
        assert_eq!(s.msg.as_deref(), Some("conversations.list"));
        assert_eq!(s.error.as_deref(), Some("boom"));
        assert!(s.started_at.is_some() && s.finished_at.is_some());
    }

    /// Log lines, hints and checkpoints all land in the log, unwrapped,
    /// and none of them disturbs the step's message line.
    #[tokio::test]
    async fn logs_hints_and_checkpoints_land_in_the_log() {
        let (td, snap) = run(&[
            Event::StepStart {
                step: "slack/raw".into(),
                attempt: 2,
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
            },
        ])
        .await;

        assert_eq!(snap.steps[0].msg.as_deref(), Some("conversations.list"));
        assert_eq!(metric_value(&snap, "slack/raw", "checkpoints"), Some(1));
        let log = log_after(td.path(), "run-1", Some("slack/raw"), 0, 100).await;
        assert_eq!(log.len(), 3, "{log:?}");
        assert_eq!(log[0].level, "warn");
        assert_eq!(
            log[0].ts, "2026-09-11T08:00:00Z",
            "the line's own clock wins"
        );
        assert_eq!(log[0].stream.as_deref(), Some("stderr"));
        assert_eq!(log[0].thread.as_deref(), Some("tokio-runtime-worker"));
        assert_eq!(log[0].target.as_deref(), Some("slack::http"));
        assert_eq!(log[0].fields.as_deref(), Some(r#"{"retry_in":30}"#));
        assert_eq!(log[1].fields.as_deref(), Some(r#"{"hint":true}"#));
        assert_eq!(log[2].msg, "sealed a checkpoint");
        assert!(
            log.iter().all(|l| l.attempt == 2),
            "every line names the pass it belongs to"
        );
    }
}
