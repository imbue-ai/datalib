//! stdout NDJSON emission: progress events (the `frankweiler_dag`
//! [`Event`] schema) plus the final outcome line. One shared lock so
//! progress and outcome lines never interleave.

use std::io::Write;
use std::sync::{Arc, Mutex};

use frankweiler_dag::events::{Event, LogLevel};
use frankweiler_etl::progress::{Progress, ProgressSink};

/// What this step claims about one of its outputs. `changed`/`version`
/// are optional exactly like the wire `ArtifactState` — absent means
/// "scheduler, hash it yourself".
#[derive(Debug, Clone)]
pub struct OutputClaim {
    pub path: String,
    pub changed: Option<bool>,
    pub version: Option<String>,
}

#[derive(Clone)]
pub struct Emitter {
    step: String,
    out: Arc<Mutex<std::io::Stdout>>,
}

impl Emitter {
    pub fn new(step: String) -> Self {
        Self {
            step,
            out: Arc::new(Mutex::new(std::io::stdout())),
        }
    }

    fn line(&self, v: &serde_json::Value) {
        let mut out = self.out.lock().unwrap();
        // Best-effort: the parent may have gone away.
        if serde_json::to_writer(&mut *out, v).is_ok() {
            let _ = out.write_all(b"\n");
            let _ = out.flush();
        }
    }

    pub fn event(&self, e: &Event) {
        if let Ok(v) = serde_json::to_value(e) {
            self.line(&v);
        }
    }

    /// The final outcome line the runner's subprocess protocol parses.
    pub fn outcome(&self, outputs: &[OutputClaim], failure: Option<&str>) {
        let outs: Vec<serde_json::Value> = outputs
            .iter()
            .map(|o| {
                let mut m = serde_json::Map::new();
                m.insert("path".into(), o.path.clone().into());
                if let Some(c) = o.changed {
                    m.insert("changed".into(), c.into());
                }
                if let Some(v) = &o.version {
                    m.insert("version".into(), v.clone().into());
                }
                serde_json::Value::Object(m)
            })
            .collect();
        let mut m = serde_json::Map::new();
        m.insert("event".into(), "outcome".into());
        m.insert("outputs".into(), outs.into());
        if let Some(f) = failure {
            m.insert("failure".into(), f.into());
        }
        self.line(&serde_json::Value::Object(m));
    }

    /// An etl-side [`Progress`] handle whose sink forwards onto this
    /// emitter — the bridge that lets every existing provider report
    /// through the DAG event stream unmodified.
    pub fn progress(&self) -> Progress {
        Progress::new(Arc::new(EmitterSink {
            emitter: self.clone(),
            step: self.step.clone(),
        }))
    }
}

/// [`ProgressSink`] impl over an [`Emitter`]. Children get a
/// `parent/child` step label, mirroring `TracingSink`.
struct EmitterSink {
    emitter: Emitter,
    step: String,
}

impl ProgressSink for EmitterSink {
    fn set_length(&self, total: Option<u64>) {
        self.emitter.event(&Event::ProgressLength {
            step: self.step.clone(),
            total,
        });
    }
    fn inc(&self, delta: u64) {
        self.emitter.event(&Event::ProgressInc {
            step: self.step.clone(),
            delta,
        });
    }
    fn set_message(&self, msg: &str) {
        self.emitter.event(&Event::ProgressMessage {
            step: self.step.clone(),
            msg: msg.to_string(),
        });
    }
    fn finish(&self, msg: &str) {
        self.emitter.event(&Event::Log {
            step: self.step.clone(),
            level: LogLevel::Info,
            msg: format!("finish: {msg}"),
        });
    }
    fn child(&self, prefix: &str) -> Arc<dyn ProgressSink> {
        Arc::new(EmitterSink {
            emitter: self.emitter.clone(),
            step: format!("{}/{}", self.step, prefix),
        })
    }
}
