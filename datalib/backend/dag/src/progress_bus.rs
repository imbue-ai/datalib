//! Publishing the event stream to the progress bus.
//!
//! The bus ([`datalib_progress`]) holds one row per step: the current
//! state, an absolute `done`/`total`, and the step's own last message.
//! The event stream is the only place progress ticks exist, so this is
//! the adapter between them.
//!
//! # Why the accumulator is here and not in the bus
//!
//! [`Event::ProgressInc`] carries a **delta**, not a position. The bus
//! coalesces — of the ticks that arrive between two flushes, only the
//! newest is written — and coalescing deltas would silently lose work,
//! turning "347 of 900" into whatever fraction of the increments
//! happened to land on a flush boundary.
//!
//! So the running total is kept here, per step, and what reaches the
//! bus is always an absolute position. Dropping one of those is
//! lossless, which is what makes the coalescing correct rather than
//! merely cheap.

use std::collections::HashMap;
use std::sync::Mutex;

use datalib_progress::{ProgressRow, ProgressWriter};

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

    /// Apply `f` to a step's accumulator, then publish the result.
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
                    self.update(step, |a| a.state = "pending".into());
                }
            }
            // A retry re-runs the step from zero, so the counters reset
            // with it — otherwise attempt 2 would appear to start
            // wherever attempt 1 died.
            Event::StepStart { step, .. } => self.update(step, |a| {
                *a = Acc {
                    state: "running".into(),
                    ..Default::default()
                }
            }),
            Event::StepFinish { step, status, .. } => {
                self.update(step, |a| a.state.clone_from(status))
            }
            Event::ProgressLength { step, total } => self.update(step, |a| a.total = *total),
            Event::ProgressInc { step, delta } => {
                self.update(step, |a| a.done = a.done.saturating_add(*delta))
            }
            Event::ProgressMessage { step, msg } => {
                self.update(step, |a| a.msg = Some(msg.clone()))
            }
            // Logs, hints and the run summary are the stream's business,
            // not the bus's. The bus answers "what is happening now".
            Event::Log { .. } | Event::Hint { .. } | Event::RunSummary { .. } => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use datalib_progress::snapshot;

    /// Drive a sink, then let it flush and read the bus back.
    async fn run(events: &[Event]) -> Vec<datalib_progress::ProgressRow> {
        let td = tempfile::tempdir().unwrap();
        {
            let sink = ProgressBusSink::start(td.path(), "run-1").expect("start the bus");
            for e in events {
                sink.emit(e);
            }
        } // dropping the sink drops the writer, which flushes and joins
        snapshot(td.path()).await
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
        assert_eq!(rows[0].state, "running");
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
        assert!(rows.iter().all(|r| r.state == "pending"));
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
                status: "succeeded".into(),
                error: None,
            },
        ])
        .await;

        assert_eq!(rows[0].state, "succeeded");
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
