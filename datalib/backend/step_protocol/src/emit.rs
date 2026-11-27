//! Printing the protocol's lines: one JSON object per line, flushed as it
//! is written, so the runner reads each one as it happens.

use std::io::Write;
use std::sync::{Arc, Mutex};

use serde::Serialize;

use crate::event::{Event, LogLevel};
use crate::outcome::Outcome;
use crate::problems::{Covered, Problem};

/// Prints a step's lines, by default on stdout. Clones share one writer,
/// and each line is written whole under a lock, so threads can print at
/// once without mixing their lines.
#[derive(Clone)]
pub struct Emitter {
    step: Arc<str>,
    out: Arc<Mutex<Box<dyn Write + Send>>>,
}

#[derive(Serialize)]
struct Tagged<'a, T: Serialize> {
    #[serde(skip_serializing_if = "Option::is_none")]
    event: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    step: Option<&'a str>,
    #[serde(flatten)]
    body: &'a T,
}

impl Emitter {
    /// Prints on stdout. `step` fills each event's `step` field; the
    /// runner replaces it with the step's real id, so any string will do.
    pub fn stdout(step: impl Into<String>) -> Emitter {
        Emitter::to_writer(step, std::io::stdout())
    }

    pub fn to_writer(step: impl Into<String>, out: impl Write + Send + 'static) -> Emitter {
        Emitter {
            step: Arc::from(step.into()),
            out: Arc::new(Mutex::new(Box::new(out))),
        }
    }

    pub fn event(&self, event: &Event) -> std::io::Result<()> {
        self.line(&Tagged {
            event: None,
            step: Some(&self.step),
            body: event,
        })
    }

    pub fn metric(&self, name: &str, labels: &[(&str, &str)], value: i64) -> std::io::Result<()> {
        self.event(&Event::Metric {
            name: name.to_string(),
            labels: labels
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            value,
        })
    }

    pub fn progress_length(&self, total: Option<u64>) -> std::io::Result<()> {
        self.event(&Event::ProgressLength { total })
    }

    pub fn progress_inc(&self, delta: u64) -> std::io::Result<()> {
        self.event(&Event::ProgressInc { delta })
    }

    pub fn progress_message(&self, msg: impl Into<String>) -> std::io::Result<()> {
        self.event(&Event::ProgressMessage { msg: msg.into() })
    }

    pub fn log(&self, level: LogLevel, msg: impl Into<String>) -> std::io::Result<()> {
        self.event(&Event::log(level, msg))
    }

    pub fn hint(&self, msg: impl Into<String>) -> std::io::Result<()> {
        self.event(&Event::Hint { msg: msg.into() })
    }

    pub fn checkpoint(&self, version: impl Into<String>, rows: Option<u64>) -> std::io::Result<()> {
        self.event(&Event::Checkpoint {
            version: version.into(),
            rows,
        })
    }

    /// `streams_output` is only ever claimed: a line that leaves it false
    /// does not take back an earlier claim.
    pub fn capabilities(
        &self,
        streams_output: bool,
        version: Option<String>,
    ) -> std::io::Result<()> {
        self.event(&Event::Capabilities {
            streams_output,
            version,
        })
    }

    pub fn problem(&self, problem: &Problem) -> std::io::Result<()> {
        self.line(&Tagged {
            event: Some("problem"),
            step: None,
            body: problem,
        })
    }

    pub fn covered(&self, covered: &Covered) -> std::io::Result<()> {
        self.line(&Tagged {
            event: Some("covered"),
            step: None,
            body: covered,
        })
    }

    /// The last line a step prints.
    pub fn outcome(&self, outcome: &Outcome) -> std::io::Result<()> {
        self.line(&Tagged {
            event: Some("outcome"),
            step: None,
            body: outcome,
        })
    }

    fn line<T: Serialize>(&self, line: &T) -> std::io::Result<()> {
        let mut bytes = serde_json::to_vec(line)?;
        bytes.push(b'\n');
        let mut out = self
            .out
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        out.write_all(&bytes)?;
        out.flush()
    }
}
