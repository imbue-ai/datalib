//! The step contract: what a step declares ([`StepSpec`]), how it is
//! invoked ([`StepRun`], [`StepCtx`]), and what it reports back
//! ([`StepOutcome`] / [`StepError`]).
//!
//! Everything not declared here is private to the step — resume
//! cursors, dedup indexes, retry bookkeeping all live behind the
//! step's own artifacts. The scheduler relies only on the advertised
//! guarantees: idempotent re-invocation, atomic outputs, and honest
//! version reporting.
//!
//! A step reports one *version string* per output it has something to
//! say about. The version is meant to be derived from the output's
//! content (a dolt commit hash, a row-set hash, a cursor hash), so
//! "unchanged" is something the scheduler *derives* — two runs over
//! the same data report the same string — rather than something the
//! step asserts. An output the step says nothing about is content
//! hashed by the scheduler instead: always correct, just slower.

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
    /// Optional author-declared version of the step's own behavior,
    /// for steps whose output can change without their command line
    /// changing — a renderer whose formatting was reworked, say. It
    /// feeds the fingerprint, so bumping it re-runs the step once.
    /// Most steps leave this `None`: argv already covers `params`.
    pub code_version: Option<String>,
}

impl StepSpec {
    /// Everything about this step except the *contents* of what it
    /// reads, as the bytes its fingerprint is taken over: the command it
    /// runs, its environment overrides, and the artifact patterns it
    /// declares — inputs included, since editing an `inputs =` line
    /// changes what the step is.
    ///
    /// This is what makes a config edit invalidate a step. A step whose
    /// `params` changed has different argv (the runner appends
    /// `--params JSON`), so it fingerprints differently and is stale
    /// even though its inputs did not move. In-process steps have no
    /// argv; they contribute their id, which is enough for tests and
    /// for the built-in steps the runner synthesizes.
    pub fn fingerprint_material(&self) -> String {
        let mut m = String::new();
        m.push_str(&self.id);
        m.push('\u{1}');
        for i in &self.inputs {
            m.push_str(i.as_str());
            m.push('\u{2}');
        }
        m.push('\u{1}');
        for o in &self.outputs {
            m.push_str(o.as_str());
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
            outputs: Vec::new(),
            run,
            code_version: None,
        }
    }

    /// Declare a version for the step's own behavior. See
    /// [`StepSpec::code_version`].
    pub fn code_version(mut self, v: impl Into<String>) -> Self {
        self.code_version = Some(v.into());
        self
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
    /// The subset of `inputs` whose version differs from the one this
    /// step consumed at its last success. Empty when the step has no
    /// last success to compare against — it is running because it has
    /// never completed, or because its own definition changed, so
    /// "what moved" has no meaning and the step should do all its work.
    pub changed_inputs: Vec<ArtifactPat>,
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

/// Per-output report: the content version of this artifact now.
/// `path` must be one of the step's declared outputs.
///
/// The version is opaque to the scheduler — it only ever compares it
/// for equality with the version a consumer recorded. What matters is
/// that it is a function of the output's *content*: a step that ran
/// twice over the same data must report the same string both times, or
/// consumers re-run for nothing. A dolt commit hash, a row-set hash,
/// or a render cursor's hash all qualify; a timestamp does not.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactState {
    pub path: ArtifactPat,
    /// Content version the step vouches for.
    pub version: String,
}

impl ArtifactState {
    pub fn versioned(path: &ArtifactPat, version: impl Into<String>) -> Self {
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
