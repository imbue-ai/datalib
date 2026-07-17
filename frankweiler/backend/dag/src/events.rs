//! The uniform per-step event stream: progress, logs, and lifecycle.
//!
//! Steps only *emit* events; the orchestrator owns all rendering
//! (terminal bars, dashboard, whatever). The schema matches what
//! `TracingSink` already emits in-process today, so the same stream
//! crosses a process boundary as NDJSON — that is exactly the
//! subprocess protocol in [`crate::subprocess`].

use std::io::Write;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::step::StepId;

/// One event on the stream. `step` tags every event so a single
/// multiplexed stream (the orchestrator's view) stays attributable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// First event of a run: every step id, in topological order. Lets
    /// a consumer render the full task board (with pending cells)
    /// before anything has started.
    RunPlan {
        steps: Vec<StepId>,
    },
    /// The scheduler decided to run this step.
    StepStart {
        step: StepId,
        attempt: u32,
    },
    /// Terminal state for the step this run. `status` is the
    /// serialized [`crate::scheduler::StepStatus`] discriminant.
    StepFinish {
        step: StepId,
        status: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Total expected work units, if known (`None` → indeterminate).
    ProgressLength {
        step: StepId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        total: Option<u64>,
    },
    ProgressInc {
        step: StepId,
        delta: u64,
    },
    ProgressMessage {
        step: StepId,
        msg: String,
    },
    Log {
        step: StepId,
        level: LogLevel,
        msg: String,
    },
    /// Actionable remediation text for a failure (e.g. the latchkey
    /// re-auth walkthrough on a 401/403). Distinct from `Log` so a UI
    /// can surface it prominently instead of burying it in the log.
    Hint {
        step: StepId,
        msg: String,
    },
    /// One terminal event per run, emitted by the scheduler after
    /// every step has a status: the whole run report, machine
    /// readable. This replaces the old `sync_summary_*.json` file —
    /// callers that want a persisted record tee the stream.
    RunSummary {
        steps: Vec<StepSummary>,
    },
}

/// Per-step entry in [`Event::RunSummary`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StepSummary {
    pub step: StepId,
    /// `succeeded` | `skipped_up_to_date` | `blocked` | `failed`.
    pub status: String,
    /// Failure kind when `status == "failed"` (wire values of
    /// `FailureKind`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<String>,
    /// Invocations this run (0 when skipped/blocked).
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub outputs: Vec<OutputSummary>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputSummary {
    pub path: String,
    pub version: String,
    pub changed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

/// Where events go. Object-safe so the orchestrator can fan out to a
/// terminal renderer + an NDJSON file + tests' recorders.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: &Event);
}

/// Discards everything.
pub struct NoopSink;
impl EventSink for NoopSink {
    fn emit(&self, _event: &Event) {}
}

/// Serializes each event as one JSON line. This is both the on-disk
/// log format and the wire format a subprocess step writes on stdout.
pub struct NdjsonSink<W: Write + Send> {
    w: Mutex<W>,
}

impl<W: Write + Send> NdjsonSink<W> {
    pub fn new(w: W) -> Self {
        Self { w: Mutex::new(w) }
    }
}

impl<W: Write + Send> EventSink for NdjsonSink<W> {
    fn emit(&self, event: &Event) {
        let mut w = self.w.lock().unwrap();
        // Best-effort: progress is observability, never load-bearing.
        if serde_json::to_writer(&mut *w, event).is_ok() {
            let _ = w.write_all(b"\n");
            let _ = w.flush();
        }
    }
}

/// Fan one emission out to several sinks.
pub struct FanOutSink(pub Vec<Arc<dyn EventSink>>);
impl EventSink for FanOutSink {
    fn emit(&self, event: &Event) {
        for s in &self.0 {
            s.emit(event);
        }
    }
}

/// The handle a step body holds: an [`EventSink`] pre-tagged with the
/// step's id. Mirrors `frankweiler_etl::progress::Progress`, so
/// bridging the existing `ProgressSink` plumbing onto this is a thin
/// adapter.
#[derive(Clone)]
pub struct StepProgress {
    step: StepId,
    sink: Arc<dyn EventSink>,
}

impl StepProgress {
    pub fn new(step: StepId, sink: Arc<dyn EventSink>) -> Self {
        Self { step, sink }
    }
    pub fn noop(step: StepId) -> Self {
        Self::new(step, Arc::new(NoopSink))
    }
    pub fn set_length(&self, total: Option<u64>) {
        self.sink.emit(&Event::ProgressLength {
            step: self.step.clone(),
            total,
        });
    }
    pub fn inc(&self, delta: u64) {
        self.sink.emit(&Event::ProgressInc {
            step: self.step.clone(),
            delta,
        });
    }
    pub fn message(&self, msg: impl Into<String>) {
        self.sink.emit(&Event::ProgressMessage {
            step: self.step.clone(),
            msg: msg.into(),
        });
    }
    pub fn log(&self, level: LogLevel, msg: impl Into<String>) {
        self.sink.emit(&Event::Log {
            step: self.step.clone(),
            level,
            msg: msg.into(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `Write` into a shared buffer so the test can read back what the
    /// sink wrote.
    #[derive(Clone, Default)]
    struct SharedBuf(Arc<Mutex<Vec<u8>>>);
    impl Write for SharedBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn ndjson_sink_writes_one_json_object_per_line() {
        let buf = SharedBuf::default();
        let sink = NdjsonSink::new(buf.clone());
        let p = StepProgress::new("slack.download".into(), Arc::new(sink));
        p.set_length(Some(42));
        p.inc(1);
        p.message("conversations.list page 1");

        let text = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        let events: Vec<Event> = text
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(events.len(), 3);
        assert!(matches!(
            events[0],
            Event::ProgressLength {
                total: Some(42),
                ..
            }
        ));
    }

    #[test]
    fn event_json_shape_matches_doc() {
        let e = Event::ProgressInc {
            step: "slack.download".into(),
            delta: 1,
        };
        let j = serde_json::to_value(&e).unwrap();
        assert_eq!(j["event"], "progress_inc");
        assert_eq!(j["step"], "slack.download");
        assert_eq!(j["delta"], 1);

        let back: Event = serde_json::from_value(j).unwrap();
        match back {
            Event::ProgressInc { step, delta } => {
                assert_eq!(step, "slack.download");
                assert_eq!(delta, 1);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }
}
