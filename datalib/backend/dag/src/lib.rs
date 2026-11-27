//! Prototype DAG runner for the pipeline architecture described in
//! `dag-runner.md` (+ addendum). An in-process (optionally
//! local-subprocess) scheduler over disk artifacts — no cluster, no
//! scheduler service.

pub mod artifact;
pub mod check;
pub mod cli;
pub mod config;
pub mod config_array;
pub mod config_lex;
pub mod config_order;
pub mod diagnostics;
pub mod disk_space;
pub mod events;
pub mod graph;
pub mod heavy_statements;
pub mod input_pattern;
pub mod memory;
mod problems_relay;
pub mod runs_sink;
pub mod scheduler;
pub mod slow_statements;
pub mod starter;
pub mod step;
pub mod subprocess;
pub mod supervisor;
pub mod version;
pub mod written;

pub use artifact::ArtifactPath;
pub use diagnostics::{Diagnostic, EntryKind, EntryRef, Severity};
pub use events::{Event, EventSink, NdjsonSink, StepProgress};
pub use graph::Graph;
pub use scheduler::{RunReport, Runner, StepReport, StepStatus};
pub use step::{ArtifactState, FailureKind, StepCtx, StepError, StepOutcome, StepRun, StepSpec};
