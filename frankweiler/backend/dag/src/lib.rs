//! Prototype DAG runner for the pipeline architecture described in
//! `dag-runner.md` (+ addendum). An in-process (optionally
//! local-subprocess) scheduler over disk artifacts — no cluster, no
//! scheduler service.
//!
//! Vocabulary (see the addendum's terminology question):
//!
//! * **Step** — the unit the scheduler schedules: a process that reads
//!   input artifacts and writes output artifacts. The design doc calls
//!   this a "node"; we use *step* here because "node" reads as data
//!   while this is an action. (Airflow/Luigi/Prefect say "task", which
//!   collides with `tokio::task` everywhere in this workspace.)
//! * **Artifact** — materialized data at rest under `data_root`,
//!   addressed by relative path. A path names the whole tree below it.
//!
//! The DAG is never declared explicitly: edges are derived from the
//! overlap of one step's declared `outputs` with another's `inputs`,
//! the way a build system derives its graph from declared deps.
//!
//! The step contract, per the doc:
//!
//! * A step is idempotent & resumable — the scheduler's retry story is
//!   "re-invoke it"; the step promises that's safe.
//! * Outputs are content-stable and atomic (valid-or-absent).
//! * After running, a step reports per output artifact whether it
//!   changed (and optionally a content version). If it reports
//!   nothing, the scheduler computes a content hash itself.
//! * A step that is scheduled because an input changed may still
//!   decide internally there is nothing to do and report unchanged
//!   outputs; dependents are then skipped.
//!
//! Module map:
//!
//! * [`artifact`] — artifact path patterns + overlap matching (the
//!   edge-derivation primitive, incl. `*`/`**` wildcard inputs).
//! * [`step`] — [`StepSpec`], [`StepRun`], [`StepOutcome`],
//!   [`FailureKind`]: the declared contract.
//! * [`graph`] — edge derivation, output-ownership conflicts, cycle
//!   detection, topological order.
//! * [`state`] — persisted `step id → input/output versions` so
//!   change detection survives across runs.
//! * [`version`] — default content-hash for artifacts whose producer
//!   doesn't report a version.
//! * [`events`] — the NDJSON progress/log event stream and sinks.
//! * [`scheduler`] — the runner: waves of ready steps, bounded
//!   parallelism, skip-if-unchanged, per-failure-kind retry, and
//!   subtree poisoning.
//! * [`subprocess`] — `StepRun::Subprocess` execution: NDJSON events
//!   on the child's stdout, outcome as the final event.
//! * [`config`] — the user-facing DAG config file: steps declared
//!   directly (there is no macro layer; the old stanza-based config
//!   format is replaced by this), with `step:`-typed entries expanded
//!   to `datalib-step` subprocess invocations by the `datalib-dag`
//!   runner binary.

pub mod artifact;
pub mod config;
pub mod events;
pub mod graph;
pub mod scheduler;
pub mod state;
pub mod step;
pub mod subprocess;
pub mod version;

pub use artifact::ArtifactPat;
pub use events::{Event, EventSink, NdjsonSink, StepProgress};
pub use graph::Graph;
pub use scheduler::{RunReport, Runner, StepReport, StepStatus};
pub use step::{ArtifactState, FailureKind, StepCtx, StepError, StepOutcome, StepRun, StepSpec};
