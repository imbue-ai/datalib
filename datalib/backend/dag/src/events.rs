//! The uniform per-step event stream: progress, logs, and lifecycle.

use std::collections::BTreeMap;
use std::io::Write;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use crate::run_state::RunState;
use crate::step::{FailureKind, StepId};

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
    /// Terminal state for the step this run.
    StepFinish {
        step: StepId,
        status: RunState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// What this step's *sink* can do, announced once as it starts.
    ///
    /// Separate from [`Event::Checkpoint`], and deliberately: a step may
    /// seal for durability — so a killed run keeps what it had — without
    /// its output being safe to read while it is still being written.
    /// Those are different claims, and collapsing them would let a
    /// consumer read a sink that cannot be read.
    ///
    /// Announced rather than probed. The alternative was running each
    /// step's command a second time with a `--capabilities` flag, which
    /// for a third-party step that ignores unknown flags would start the
    /// real work. A step is already going to run; it can say this on the
    /// way past.
    Capabilities {
        step: StepId,
        /// P2 of the sink contract in `docs/dev/streaming_steps_plan.md`:
        /// may a consumer read this output while it is being written?
        streams_output: bool,
    },
    /// A step sealed part of its output and is still running.
    ///
    /// `version` is the same kind of string the terminal `outcome` reports —
    /// a content version the step vouches for — so a consumer comparing them
    /// needs no new vocabulary. There is no `path`: a step has exactly one
    /// output, and its id *is* that path.
    ///
    /// **A checkpoint is a hint, never an obligation.** A consumer that
    /// ignores every one of them does a single pass at the end and is
    /// correct, just later. That is what keeps a dropped notification a
    /// performance question rather than a correctness one, and it is worth
    /// protecting: the moment something *needs* these to be right, we have
    /// built Kafka's worst failure mode into our own storage engine.
    Checkpoint {
        step: StepId,
        version: String,
    },
    /// The current value of one of the step's metrics: rows written,
    /// requests made, items still queued. Always an absolute value — the
    /// store coalesces to the newest, and a dropped delta would be lost
    /// work while a dropped position is nothing. A value that goes down
    /// is simply a gauge; nothing on the wire distinguishes the two.
    Metric {
        step: StepId,
        name: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        labels: BTreeMap<String, String>,
        value: i64,
    },
    /// Total expected work units, if known (`None` → indeterminate).
    /// Sugar for a step that counts one thing: the runner turns this and
    /// [`Event::ProgressInc`] into the `done` and `queued` metrics.
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
    /// One log line. A line that arrived as structured tracing output
    /// is unwrapped here: `msg` is its message, `target` its target, and
    /// `fields` whatever else it carried — so no reader has to parse a
    /// JSON envelope out of a string a second time.
    Log {
        step: StepId,
        level: LogLevel,
        msg: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        target: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fields: Option<serde_json::Map<String, serde_json::Value>>,
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
    pub status: RunState,
    /// Set when `status` is [`RunState::Failed`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<FailureKind>,
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

#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

impl LogLevel {
    pub fn as_str(self) -> &'static str {
        self.into()
    }
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

/// One emitted line: the event, plus the wall-clock instant the sink saw it.
#[derive(Serialize)]
struct Stamped<'a> {
    ts: String,
    #[serde(flatten)]
    event: &'a Event,
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
        let stamped = Stamped {
            ts: datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339(),
            event,
        };
        let mut w = self.w.lock().unwrap();
        // Best-effort: progress is observability, never load-bearing.
        if serde_json::to_writer(&mut *w, &stamped).is_ok() {
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
/// step's id. Mirrors `datalib_etl::progress::Progress`, so
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
    pub fn metric(&self, name: &str, labels: BTreeMap<String, String>, value: i64) {
        self.sink.emit(&Event::Metric {
            step: self.step.clone(),
            name: name.to_string(),
            labels,
            value,
        });
    }
    pub fn log(&self, level: LogLevel, msg: impl Into<String>) {
        self.sink.emit(&Event::Log {
            step: self.step.clone(),
            level,
            msg: msg.into(),
            target: None,
            fields: None,
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

    /// The wire shape `step_protocol.md` documents, with the labels map
    /// dropped when empty so a step counting one thing writes the short
    /// form.
    #[test]
    fn metric_event_json_shape_matches_doc() {
        let e = Event::Metric {
            step: "slack/ingest".into(),
            name: "rows_upserted".into(),
            labels: BTreeMap::from([("table".to_string(), "slack_messages".to_string())]),
            value: 1234,
        };
        let j = serde_json::to_value(&e).unwrap();
        assert_eq!(j["event"], "metric");
        assert_eq!(j["labels"]["table"], "slack_messages");
        assert_eq!(j["value"], 1234);

        let bare: Event = serde_json::from_str(
            r#"{"event":"metric","step":"s","name":"api_requests","value":7}"#,
        )
        .unwrap();
        match bare {
            Event::Metric { labels, value, .. } => {
                assert!(labels.is_empty());
                assert_eq!(value, 7);
            }
            other => panic!("wrong variant: {other:?}"),
        }
    }

    /// strum and serde spell the levels independently; the store writes
    /// the strum one and a reader matches the serde one.
    #[test]
    fn log_level_strum_and_serde_agree() {
        for &l in <LogLevel as strum::VariantArray>::VARIANTS {
            assert_eq!(
                serde_json::to_string(&l).unwrap(),
                format!("\"{}\"", l.as_str())
            );
        }
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
