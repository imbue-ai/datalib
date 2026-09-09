//! The step contract: what a step declares ([`StepSpec`]), how it is
//! invoked ([`StepRun`], [`StepCtx`]), and what it reports back
//! ([`StepOutcome`] / [`StepError`]).

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::artifact::ArtifactPath;
use crate::events::StepProgress;

pub type StepId = String;

/// A step declaration.
#[derive(Debug, Clone)]
pub struct StepSpec {
    /// Identity, and the tree this step writes: `<data_root>/<id>/`.
    /// Unique across the config, which is what makes single-writer true
    /// by construction.
    pub id: StepId,
    /// The ids of the steps this one reads. Every entry must name a
    /// declared step; the loader refuses anything else.
    pub inputs: Vec<ArtifactPath>,
    /// How to run it. In-process today; a spawned subprocess under the
    /// same contract.
    pub run: StepRun,
    /// Optional author-declared version of the step's own behavior,
    /// for steps whose output can change without their command line
    /// changing — a renderer whose formatting was reworked, say. It
    /// feeds the fingerprint, so bumping it re-runs the step once.
    /// Most steps leave this `None`: argv already covers `params`.
    pub code_version: Option<String>,
    /// Whether a consumer may read this step's output *while it is still
    /// being written* — P2 of the sink contract in
    /// `docs/dev/streaming_steps_plan.md`.
    ///
    /// Default `false`, and deliberately so: most sinks cannot, and the
    /// failure when they cannot is a consumer acting on a torn read rather
    /// than anything that looks like an error. A step earns this by having
    /// a sink that can hand out a stable view under a live writer — a
    /// doltlite store does, an index rewritten in place does not.
    ///
    /// Not in `fingerprint_material`: it changes when a consumer may run,
    /// never what this step produces, so flipping it should not re-run
    /// anything.
    pub streams_output: bool,
}

impl StepSpec {
    /// Everything about this step except the *contents* of what it
    /// reads, as the bytes its fingerprint is taken over: its id (which
    /// is also the tree it writes), the command it runs, its
    /// environment overrides, and the step ids it declares as inputs —
    /// since editing an `inputs =` line changes what the step is.
    pub fn fingerprint_material(&self) -> String {
        let mut m = String::new();
        m.push_str(&self.id);
        m.push('\u{1}');
        for i in &self.inputs {
            m.push_str(i.as_str());
            m.push('\u{2}');
        }
        m.push('\u{1}');
        m.push_str(self.code_version.as_deref().unwrap_or(""));
        m.push('\u{1}');
        match &self.run {
            StepRun::InProcess(_) => m.push_str("in-process"),
            StepRun::Subprocess { argv, env } => {
                for a in argv {
                    m.push_str(a);
                    m.push('\u{2}');
                }
                m.push('\u{1}');
                for (k, v) in env {
                    m.push_str(k);
                    m.push('=');
                    m.push_str(v);
                    m.push('\u{2}');
                }
            }
        }
        m
    }

    pub fn new(id: impl Into<String>, run: StepRun) -> Self {
        Self {
            id: id.into(),
            inputs: Vec::new(),
            run,
            code_version: None,
            streams_output: false,
        }
    }

    /// The one tree this step writes, which is its id. Panics only on
    /// an id that never passed the loader's validation.
    pub fn output(&self) -> ArtifactPath {
        ArtifactPath::parse(&self.id).expect("step id is a valid artifact path")
    }

    pub fn code_version(mut self, v: impl Into<String>) -> Self {
        self.code_version = Some(v.into());
        self
    }

    /// Declare that this step's output can be read while it is being
    /// written, so consumers may be dispatched on its checkpoints.
    pub fn streams_output(mut self) -> Self {
        self.streams_output = true;
        self
    }

    pub fn input(mut self, id: &str) -> Self {
        self.inputs
            .push(ArtifactPath::parse(id).expect("input step id"));
        self
    }
}

pub type StepFuture = Pin<Box<dyn Future<Output = Result<StepOutcome, StepError>> + Send>>;
pub type StepFn = Arc<dyn Fn(StepCtx) -> StepFuture + Send + Sync>;

/// How a step is executed. The contract is identical either way; the
/// subprocess variant buys isolation and language-independence.
#[derive(Clone)]
pub enum StepRun {
    InProcess(StepFn),
    Subprocess {
        argv: Vec<String>,
        env: BTreeMap<String, String>,
    },
}

impl StepRun {
    pub fn in_process<F, Fut>(f: F) -> Self
    where
        F: Fn(StepCtx) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<StepOutcome, StepError>> + Send + 'static,
    {
        StepRun::InProcess(Arc::new(move |ctx| Box::pin(f(ctx))))
    }
}

impl std::fmt::Debug for StepRun {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StepRun::InProcess(_) => f.write_str("InProcess(..)"),
            StepRun::Subprocess { argv, .. } => write!(f, "Subprocess({argv:?})"),
        }
    }
}

/// Where a step says it sealed part of its output, so a consumer could
/// start on it before the step finishes.
///
/// One mechanism for both kinds of step: an in-process step calls
/// [`StepCtx::checkpoint`] directly, and `run_subprocess` calls it when a
/// child's NDJSON carries a `checkpoint` event. Cloneable and cheap, and
/// [`CheckpointSink::disconnected`] is a working sink that drops
/// everything — which is what a step invoked outside the scheduler gets.
#[derive(Clone, Default, Debug)]
pub struct CheckpointSink(Option<tokio::sync::mpsc::UnboundedSender<StepSignal>>);

/// Something a running step tells the scheduler, as opposed to the event
/// stream. These change what the runner *does* next, not just what it shows.
#[derive(Debug, Clone)]
pub enum StepSignal {
    /// What this step's sink can do. Sent once, as the step starts.
    Capabilities { step: StepId, streams_output: bool },
    /// The step sealed its output at `version` and is still running.
    Checkpoint { step: StepId, version: String },
}

impl CheckpointSink {
    pub fn new(tx: tokio::sync::mpsc::UnboundedSender<StepSignal>) -> Self {
        Self(Some(tx))
    }

    /// Announces nothing. Not an error: a step run outside a scheduler --
    /// a test, a one-shot CLI invocation -- has nobody to tell.
    pub fn disconnected() -> Self {
        Self(None)
    }

    pub fn send(&self, step: &StepId, version: &str) {
        self.signal(StepSignal::Checkpoint {
            step: step.clone(),
            version: version.to_string(),
        });
    }

    /// Say what this step's sink can do. See [`StepSignal::Capabilities`].
    pub fn declare(&self, step: &StepId, streams_output: bool) {
        self.signal(StepSignal::Capabilities {
            step: step.clone(),
            streams_output,
        });
    }

    /// Best-effort. A closed receiver means the run is already tearing
    /// down, and a producer should not fail because nobody is listening.
    fn signal(&self, s: StepSignal) {
        if let Some(tx) = self.0.as_ref() {
            let _ = tx.send(s);
        }
    }
}

/// Everything a running step gets from the scheduler. Steps resolve
/// their own paths under `data_root`; `inputs`/`changed_inputs` let a
/// step narrow its work to what actually moved without re-deriving the
/// graph.
#[derive(Clone)]
pub struct StepCtx {
    pub step_id: StepId,
    pub data_root: PathBuf,
    /// Concrete input artifacts, resolved from the step's input
    /// patterns (producer outputs + external artifacts), relative to
    /// `data_root`.
    pub inputs: Vec<ArtifactPath>,
    /// The subset of `inputs` whose version differs from the one this
    /// step consumed at its last success. Empty when the step has no
    /// last success to compare against — it is running because it has
    /// never completed, or because its own definition changed, so
    /// "what moved" has no meaning and the step should do all its work.
    pub changed_inputs: Vec<ArtifactPath>,
    /// Progress/log emitter, already tagged with this step's id.
    pub progress: StepProgress,
    /// Where to announce a seal. See [`StepCtx::checkpoint`].
    pub checkpoint: CheckpointSink,
}

impl StepCtx {
    pub fn path(&self, artifact: &ArtifactPath) -> PathBuf {
        self.data_root.join(artifact.as_str())
    }

    pub fn path_str(&self, rel: &str) -> PathBuf {
        self.data_root.join(rel)
    }

    /// Say that this step's output is readable up to `version`.
    ///
    /// Only call it where the output is *consistent* — a consumer may be
    /// dispatched against it immediately. A step whose output cannot be
    /// read while it is being written must never call this; that is P2 of
    /// the sink contract, and it is declared by
    /// [`StepSpec::streams_output`].
    pub fn checkpoint(&self, version: &str) {
        self.checkpoint.send(&self.step_id, version);
    }

    /// Announce whether this step's output may be read while it is being
    /// written. Subprocess steps say it on the wire; the scheduler forwards
    /// it here. See [`StepSpec::streams_output`].
    pub fn declare_streams_output(&self, streams: bool) {
        self.checkpoint.declare(&self.step_id, streams);
    }
}

/// Per-output report: the content version of this artifact now.
/// `path` must be one of the step's declared outputs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactState {
    pub path: ArtifactPath,
    /// Content version the step vouches for.
    /// Must be a function of the output's *content* — a dolt commit hash, a
    /// row-set hash — so two runs over the same data report the same string
    /// and the scheduler can derive "unchanged". A timestamp does not
    /// qualify. Opaque otherwise: it is only ever compared for equality.
    pub version: String,
}

impl ArtifactState {
    pub fn versioned(path: &ArtifactPath, version: impl Into<String>) -> Self {
        Self {
            path: path.clone(),
            version: version.into(),
        }
    }
}

/// What a successful step reports. A declared output missing from
/// `outputs` means "I have nothing to say about this one" — the
/// scheduler content-hashes it instead. That is always correct and
/// always slower, so first-party steps report a version for every
/// output they declare.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StepOutcome {
    #[serde(default)]
    pub outputs: Vec<ArtifactState>,
}

/// Failure classification — the part of a failure the scheduler acts
/// on. The mapping to a retry policy lives in the scheduler; the step
/// only says *which kind* this is.
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
pub enum FailureKind {
    /// Try again soon (network blips, lock contention).
    Transient,
    /// Try again later, with backoff (HTTP 429 and friends).
    RateLimited,
    /// Fail fast; a human must fix credentials.
    Auth,
    /// The input/data is bad; retrying won't help. Fails this step
    /// (poisoning its subtree), not the graph.
    Data,
    /// The run was cancelled from outside.
    Cancelled,
}

impl FailureKind {
    /// The wire spelling a step writes on its `outcome` line, and the
    /// one `RunSummary` reports.
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<FailureKind> {
        s.parse().ok()
    }
}

/// A step failure. Because steps are incremental, a failed step may
/// still have committed partial output — `outputs` reports that, so
/// the scheduler records the new versions even though the step failed
/// (dependents stay blocked this run; next run sees changed inputs).
#[derive(Debug)]
pub struct StepError {
    pub kind: FailureKind,
    pub error: anyhow::Error,
    pub outputs: Vec<ArtifactState>,
}

impl StepError {
    pub fn new(kind: FailureKind, error: impl Into<anyhow::Error>) -> Self {
        Self {
            kind,
            error: error.into(),
            outputs: Vec::new(),
        }
    }
    pub fn with_outputs(mut self, outputs: Vec<ArtifactState>) -> Self {
        self.outputs = outputs;
        self
    }
}

impl std::fmt::Display for StepError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {:#}", self.kind, self.error)
    }
}

impl std::error::Error for StepError {}
