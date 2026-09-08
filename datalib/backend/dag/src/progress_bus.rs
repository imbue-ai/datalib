//! Publishing the event stream to the progress bus.
//!
//! The per-step running total lives here, not in the bus: `ProgressInc`
//! carries a delta, the bus coalesces, and coalescing deltas loses work.
//! What reaches the bus is always an absolute position, so dropping one is
//! lossless.

use std::collections::HashMap;
use std::sync::Mutex;

use datalib_progress::{LiveState, ProgressRow, ProgressWriter};

use crate::events::{Event, EventSink};
use crate::step::StepId;

/// What we know about one step right now.
#[derive(Default, Clone)]
struct Acc {
    state: String,
    done: u64,
    total: Option<u64>,
    msg: Option<String>,
}

/// An [`EventSink`] that keeps the progress bus current.
pub struct ProgressBusSink {
    writer: ProgressWriter,
    steps: Mutex<HashMap<StepId, Acc>>,
}

impl ProgressBusSink {
    /// Returns `None` when the bus could not be opened. Progress is
    /// observability: a sync that runs without drawing a bar is much
    /// better than one that refuses to start because a status file was
    /// unwritable.
    pub fn start(data_root: &std::path::Path, run_id: &str) -> Option<Self> {
        Some(Self {
            writer: ProgressWriter::start(data_root, run_id)?,
            steps: Mutex::new(HashMap::new()),
        })
    }

    fn update(&self, step: &StepId, f: impl FnOnce(&mut Acc)) {
        let row = {
            let mut steps = self.steps.lock().expect("progress bus sink mutex");
            let acc = steps.entry(step.clone()).or_default();
            f(acc);
            ProgressRow {
                step: step.clone(),
                state: acc.state.clone(),
                // `done` is meaningless before a step reports anything:
                // a bar drawn from "0 of unknown" should be a spinner,
                // not an empty bar claiming zero progress.
                done: (acc.done > 0 || acc.total.is_some()).then_some(acc.done as i64),
                total: acc.total.map(|t| t as i64),
                msg: acc.msg.clone(),
                updated_at: datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339(),
            }
        };
        self.writer.update(row);
    }
}

impl EventSink for ProgressBusSink {
    fn emit(&self, event: &Event) {
        match event {
            // Publish the whole plan up front so a reader can draw every
            // row, pending ones included, before anything has started.
            Event::RunPlan { steps } => {
                for step in steps {
                    self.update(step, |a| a.state = LiveState::Pending.as_str().into());
                }
            }
            // A retry re-runs the step from zero, so the counters reset
            // with it — otherwise attempt 2 would appear to start
            // wherever attempt 1 died.
            Event::StepStart { step, .. } => self.update(step, |a| {
                *a = Acc {
                    state: LiveState::Running.as_str().into(),
                    ..Default::default()
                }
            }),
            Event::StepFinish { step, status, .. } => {
                self.update(step, |a| a.state = status.as_str().into())
            }
            Event::ProgressLength { step, total } => self.update(step, |a| a.total = *total),
            Event::ProgressInc { step, delta } => {
                self.update(step, |a| a.done = a.done.saturating_add(*delta))
            }
            Event::ProgressMessage { step, msg } => {
                self.update(step, |a| a.msg = Some(msg.clone()))
            }
            // A checkpoint says the step sealed part of its output and is
            // still going. It is not progress — it moves no counter — but it
            // is the one thing a watcher can act on before the step ends, so
            // it reaches the message line. The absolute position stays
            // whatever the step last reported.
            Event::Checkpoint { step, .. } => self.update(step, |a| {
                a.msg = Some(if a.done > 0 {
                    format!("committed {} so far", a.done)
                } else {
                    "committed a first batch".to_string()
                })
            }),
            // Logs, hints and the run summary are the stream's business,
            // not the bus's. The bus answers "what is happening now".
            Event::Log { .. } | Event::Hint { .. } | Event::RunSummary { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::run_state::RunState;
    use datalib_progress::snapshot;

    async fn run(events: &[Event]) -> Vec<datalib_progress::ProgressRow> {
        let td = tempfile::tempdir().unwrap();
        {
            let sink = ProgressBusSink::start(td.path(), "run-1").expect("start the bus");
            for e in events {
                sink.emit(e);
            }
        } // dropping the sink drops the writer, which flushes and joins
        snapshot(td.path()).await.steps
    }

    fn inc(step: &str, delta: u64) -> Event {
        Event::ProgressInc {
            step: step.into(),
            delta,
        }
    }

    /// The point of the accumulator. The stream carries increments; the
    /// bus must carry a position, or coalescing would drop work.
    #[tokio::test]
    async fn deltas_accumulate_into_an_absolute_position() {
        let rows = run(&[
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

        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].done, Some(6), "1 + 3 + 2, not the last delta");
        assert_eq!(rows[0].total, Some(9));
        assert_eq!(rows[0].state, LiveState::Running.as_str());
    }

    /// A retry re-runs the step from the beginning, so the count has to
    /// go back to zero. Otherwise attempt 2 appears to resume from
    /// wherever attempt 1 died and can sail past `total`.
    #[tokio::test]
    async fn a_retry_restarts_the_count() {
        let rows = run(&[
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

        assert_eq!(rows[0].done, Some(1), "attempt 2 starts over, not at 8");
        assert_eq!(rows[0].total, None, "and re-learns its total");
    }

    /// A reader must be able to draw the whole table before the first
    /// step starts, which is what makes the plan event worth publishing.
    #[tokio::test]
    async fn the_plan_lands_before_anything_runs() {
        let rows = run(&[Event::RunPlan {
            steps: vec!["slack/raw".into(), "slack/rendered_md".into()],
        }])
        .await;

        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|r| r.state == LiveState::Pending.as_str()));
        assert!(
            rows.iter().all(|r| r.done.is_none()),
            "a step that has reported nothing has no position — a bar \
             drawn from this should be a spinner, not an empty bar"
        );
    }

    /// The step's own words reach the bus, and the terminal state sticks.
    #[tokio::test]
    async fn the_last_message_and_the_outcome_are_recorded() {
        let rows = run(&[
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
                status: RunState::Succeeded,
                error: None,
            },
        ])
        .await;

        assert_eq!(rows[0].state, RunState::Succeeded.as_str());
        assert_eq!(rows[0].msg.as_deref(), Some("conversations.list"));
    }

    /// Logs are the stream's job. If they landed here every log line
    /// would overwrite the step's progress message.
    #[tokio::test]
    async fn logs_do_not_disturb_the_bus() {
        let rows = run(&[
            Event::ProgressMessage {
                step: "slack/raw".into(),
                msg: "conversations.list".into(),
            },
            Event::Log {
                step: "slack/raw".into(),
                level: crate::events::LogLevel::Warn,
                msg: "rate limited, backing off".into(),
            },
        ])
        .await;

        assert_eq!(rows[0].msg.as_deref(), Some("conversations.list"));
    }
}
