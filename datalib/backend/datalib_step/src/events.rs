//! What this step prints on stdout, through the step protocol's own
//! [`datalib_step_protocol::Emitter`]: progress events, and the outcome
//! line with the answers the verbs collect while the step runs.

use std::sync::{Arc, Mutex};

use datalib_etl::progress::{Progress, ProgressSink};
use datalib_step_protocol::{Event, FailureKind, LogLevel, Outcome, Output, StoreBytes};

/// What this step claims about one of its outputs: its content version
/// now.
#[derive(Debug, Clone)]
pub struct OutputClaim {
    pub path: String,
    pub version: String,
    /// Rows the output gained since the last seal, when counted.
    pub rows: Option<u64>,
}

#[derive(Clone)]
pub struct Emitter {
    out: datalib_step_protocol::Emitter,
    /// Set by a `--prepare` that cannot bring the step's store to this
    /// build's shape in place; the outcome then says `needs_rerun`.
    needs_rerun: Arc<std::sync::atomic::AtomicBool>,
    /// Set by a `--collect` or `--compact` that shrank the step's store:
    /// its size before and after, in bytes.
    store_bytes: Arc<Mutex<Option<(u64, u64)>>>,
    /// Set by a `--check` that would run the step's params with these
    /// warnings.
    warnings: Arc<Mutex<Vec<String>>>,
}

impl Emitter {
    pub fn new(step: String) -> Self {
        Self {
            out: datalib_step_protocol::Emitter::stdout(step),
            needs_rerun: Arc::default(),
            store_bytes: Arc::default(),
            warnings: Arc::default(),
        }
    }

    #[cfg(test)]
    pub fn reported_needs_rerun(&self) -> bool {
        self.needs_rerun.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn needs_rerun(&self) {
        self.needs_rerun
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn warnings(&self, warnings: Vec<String>) {
        *self.warnings.lock().unwrap() = warnings;
    }

    pub fn store_bytes(&self, before: u64, after: u64) {
        *self.store_bytes.lock().unwrap() = Some((before, after));
    }

    // Best-effort, here and below: the runner may have gone away, and a
    // step that cannot say what it did has nothing better to do.
    pub fn event(&self, e: &Event) {
        let _ = self.out.event(e);
    }

    pub fn hint(&self, msg: String) {
        let _ = self.out.hint(msg);
    }

    /// The final stdout line. `failure` classifies a non-zero exit.
    pub fn outcome(&self, outputs: &[OutputClaim], failure: Option<FailureKind>) {
        let outcome = Outcome {
            outputs: outputs
                .iter()
                .map(|o| Output {
                    path: o.path.clone(),
                    version: Some(o.version.clone()),
                    rows: o.rows,
                })
                .collect(),
            needs_rerun: self.needs_rerun.load(std::sync::atomic::Ordering::SeqCst),
            store_bytes: self
                .store_bytes
                .lock()
                .unwrap()
                .map(|(before, after)| StoreBytes { before, after }),
            warnings: self.warnings.lock().unwrap().clone(),
            failure,
        };
        let _ = self.out.outcome(&outcome);
    }

    /// Say whether this step's output may be read while it is being
    /// written. The runner uses it to decide whether a checkpoint may
    /// dispatch a consumer early.
    pub fn declare_streams_output(&self, streams_output: bool) {
        let _ = self.out.capabilities(streams_output, this_build());
    }

    /// Say which commit this program was built from, which the run store
    /// records for the process. Streams nothing until a step says it does.
    pub fn declare_version(&self) {
        self.declare_streams_output(false);
    }

    pub fn progress(&self) -> Progress {
        Progress::new(Arc::new(EmitterSink {
            out: self.out.clone(),
        }))
    }
}

/// This build's commit, when the build knows it.
fn this_build() -> Option<String> {
    datalib_runtime::build_id::git_hash()
}

/// [`ProgressSink`] impl over the protocol's emitter.
struct EmitterSink {
    out: datalib_step_protocol::Emitter,
}

impl ProgressSink for EmitterSink {
    fn set_length(&self, total: Option<u64>) {
        let _ = self.out.progress_length(total);
    }
    fn inc(&self, delta: u64) {
        let _ = self.out.progress_inc(delta);
    }
    fn set_message(&self, msg: &str) {
        let _ = self.out.progress_message(msg);
    }
    fn checkpoint(&self, version: &str) {
        let _ = self.out.checkpoint(version, None);
    }
    fn checkpoint_rows(&self, version: &str, rows: u64) {
        let _ = self.out.checkpoint(version, Some(rows));
    }
    fn metric(&self, name: &str, labels: &[(&str, &str)], value: i64) {
        let _ = self.out.metric(name, labels, value);
    }
    fn finish(&self, msg: &str) {
        let _ = self.out.event(&Event::Log {
            level: LogLevel::Info,
            msg: format!("finish: {msg}"),
            ts: None,
            target: Some("datalib_step::progress".into()),
            thread: None,
            fields: None,
        });
    }
}
