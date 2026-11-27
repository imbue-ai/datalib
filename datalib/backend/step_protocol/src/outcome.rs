//! The `outcome` line: the last thing a step prints (`step_protocol.md`
//! §"The outcome line", §"Failure classification", and the verbs'
//! answers).

use serde::{Deserialize, Serialize};

/// The exit code of a step that stopped because it was asked to, by
/// convention: 128 + SIGINT.
pub const CANCELLED_EXIT_CODE: i32 = 130;

crate::closed_set! {
    /// What kind of failure a non-zero exit was, which decides whether the
    /// runner tries again. `Data` when the outcome names none.
    pub enum FailureKind {
        /// A network blip or lock contention: retried, up to 3 attempts.
        Transient = "transient",
        /// HTTP 429 and the like: retried as `Transient` is.
        RateLimited = "rate_limited",
        /// Credentials a person must fix: not retried.
        Auth = "auth",
        /// The OS would not let the step read a path its settings name
        /// (macOS privacy): not retried.
        Permission = "permission",
        /// Bad input that a retry will not fix: not retried.
        Data = "data",
        /// The step was asked to stop; exit with [`CANCELLED_EXIT_CODE`].
        Cancelled = "cancelled",
        /// The step gave up with work still owed, after committing what it
        /// got: not retried, and the next run resumes.
        Incomplete = "incomplete",
        /// The answer to `--prepare` when what the step wrote is in a shape
        /// this program cannot use: the runner turns the step off until a
        /// later start's `--prepare` does not refuse.
        Incompatible = "incompatible",
    }
}

/// A `--collect` or `--compact` answer: the step's store's size in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoreBytes {
    pub before: u64,
    pub after: u64,
}

/// One output's content version. A step has one output, its id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Output {
    pub path: String,
    /// A function of the output's content, compared only for equality:
    /// two runs that leave the same data report the same string. `None`
    /// makes the run count as new, as leaving the output out does.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Rows the output gained since the last checkpoint, or in all if the
    /// run sealed none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<u64>,
}

impl Output {
    pub fn versioned(path: impl Into<String>, version: impl Into<String>) -> Output {
        Output {
            path: path.into(),
            version: Some(version.into()),
            rows: None,
        }
    }
}

/// The `outcome` line. Every field may be left at its default.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Outcome {
    /// What the step committed: on success, and on failure too.
    #[serde(default)]
    pub outputs: Vec<Output>,
    /// The `--prepare` answer when what the step wrote has to be made
    /// again by a run.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub needs_rerun: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub store_bytes: Option<StoreBytes>,
    /// The `--check` answer: one sentence for each thing the step would
    /// run as written but is probably a mistake.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// Set, with a non-zero exit, to classify the failure.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub failure: Option<FailureKind>,
}

impl Outcome {
    /// A success that reports these outputs.
    pub fn with_outputs(outputs: Vec<Output>) -> Outcome {
        Outcome {
            outputs,
            ..Outcome::default()
        }
    }

    /// A failure of `kind` that reports what the step committed.
    pub fn failed(kind: FailureKind, outputs: Vec<Output>) -> Outcome {
        Outcome {
            outputs,
            failure: Some(kind),
            ..Outcome::default()
        }
    }
}
