//! The step contract: what a step declares ([`StepSpec`]), how it is
//! invoked ([`StepRun`], [`StepCtx`]), and what it reports back
//! ([`StepOutcome`] / [`StepError`]).
//!
//! Everything not declared here is private to the step — resume
//! cursors, dedup indexes, retry bookkeeping all live behind the
//! step's own artifacts. The scheduler relies only on the advertised
//! guarantees: idempotent re-invocation, atomic outputs, and honest
//! change reporting.

use std::collections::BTreeMap;
use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::artifact::ArtifactPat;
use crate::events::StepProgress;

pub type StepId = String;

/// A step declaration. Edges in the DAG are derived from the overlap
/// of one step's `outputs` with another's `inputs`; nothing else links
/// steps together.
#[derive(Debug, Clone)]
pub struct StepSpec {
    pub id: StepId,
    /// Artifacts this step reads. May contain `*`/`**` wildcards
    /// ("everything any download step produced").
    pub inputs: Vec<ArtifactPat>,
    /// Artifacts this step produces. Concrete paths only; a step MUST
    /// write only under these, and no two steps' outputs may overlap.
    pub outputs: Vec<ArtifactPat>,
    /// How to run it. In-process today; a spawned subprocess under the
    /// same contract.
    pub run: StepRun,
}

impl StepSpec {
    pub fn new(id: impl Into<String>, run: StepRun) -> Self {
        Self {
            id: id.into(),
            inputs: Vec::new(),
            outputs: Vec::new(),
            run,
        }
    }

    pub fn input(mut self, pat: &str) -> Self {
        self.inputs
            .push(ArtifactPat::parse(pat).expect("input pattern"));
        self
    }

    pub fn output(mut self, pat: &str) -> Self {
        self.outputs
            .push(ArtifactPat::parse(pat).expect("output path"));
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
    /// Wrap an async closure as an in-process step body.
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
    pub inputs: Vec<ArtifactPat>,
    /// The subset of `inputs` whose version changed since this step
    /// last succeeded. Empty on a first run (everything is new — see
    /// `first_run`).
    pub changed_inputs: Vec<ArtifactPat>,
    /// True when the scheduler has no record of a prior successful run.
    pub first_run: bool,
    /// Progress/log emitter, already tagged with this step's id.
    pub progress: StepProgress,
}

impl StepCtx {
    /// Absolute path of an artifact under `data_root`.
    pub fn path(&self, artifact: &ArtifactPat) -> PathBuf {
        self.data_root.join(artifact.as_str())
    }

    /// Absolute path for a relative artifact string (convenience for
    /// step bodies that know their own layout).
    pub fn path_str(&self, rel: &str) -> PathBuf {
        self.data_root.join(rel)
    }
}

/// Per-output report: did this artifact change, and (optionally) what
/// is its content version now. `path` must be one of the step's
/// declared outputs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactState {
    pub path: ArtifactPat,
    /// `None` → "scheduler, decide for yourself" (content hash).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed: Option<bool>,
    /// Content version the step vouches for (e.g. a row-set hash or a
    /// dolt commit hash). `None` → scheduler computes a tree hash.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

impl ArtifactState {
    pub fn changed(path: &ArtifactPat) -> Self {
        Self {
            path: path.clone(),
            changed: Some(true),
            version: None,
        }
    }
    pub fn unchanged(path: &ArtifactPat) -> Self {
        Self {
            path: path.clone(),
            changed: Some(false),
            version: None,
        }
    }
    pub fn versioned(path: &ArtifactPat, version: impl Into<String>) -> Self {
        Self {
            path: path.clone(),
            changed: None,
            version: Some(version.into()),
        }
    }
}

/// What a successful step reports. An empty `outputs` list means "I
/// have nothing to say about my outputs" — the scheduler content-hashes
/// each declared output to find out what changed.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct StepOutcome {
    #[serde(default)]
    pub outputs: Vec<ArtifactState>,
}

impl StepOutcome {
    pub fn unchanged_all() -> Self {
        // Marker resolved by the scheduler against the declared
        // outputs (we don't have them here).
        Self { outputs: vec![] }
    }
}

/// Failure classification — the part of a failure the scheduler acts
/// on. The mapping to a retry policy lives in the scheduler; the step
/// only says *which kind* this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
