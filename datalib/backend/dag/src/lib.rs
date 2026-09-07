//! Prototype DAG runner for the pipeline architecture described in
//! `dag-runner.md` (+ addendum). An in-process (optionally
//! local-subprocess) scheduler over disk artifacts — no cluster, no
//! scheduler service.

pub mod artifact;
pub mod config;
pub mod diagnostics;
pub mod events;
pub mod graph;
pub mod lock;
pub mod progress_bus;
pub mod run_state;
pub mod scheduler;
pub mod state;
pub mod step;
pub mod subprocess;
pub mod version;

pub use artifact::ArtifactPath;
pub use diagnostics::{Diagnostic, EntryKind, EntryRef, Severity};
pub use events::{Event, EventSink, NdjsonSink, StepProgress};
pub use graph::Graph;
pub use run_state::RunState;
pub use scheduler::{RunReport, Runner, StepReport, StepStatus};
pub use step::{ArtifactState, FailureKind, StepCtx, StepError, StepOutcome, StepRun, StepSpec};
