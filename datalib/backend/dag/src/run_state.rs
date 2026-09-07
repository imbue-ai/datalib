//! What a step is doing in a run, as one named vocabulary.
//!
//! These values travel on four surfaces — `Event::StepFinish.status`,
//! `Event::RunSummary`'s per-step `status`, `dag_state.json`'s
//! `current_run.states` and `last_run.status`, and the HTTP API's
//! `current_state` — and every producer and consumer has to agree on
//! the spelling. Naming them once is what keeps those copies from
//! drifting apart.

use serde::{Deserialize, Serialize};
use strum::{EnumString, IntoStaticStr, VariantArray};

/// What one step is doing, or did, in one run.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Serialize,
    Deserialize,
    EnumString,
    IntoStaticStr,
    VariantArray,
)]
#[serde(rename_all = "snake_case")]
#[strum(serialize_all = "snake_case")]
pub enum RunState {
    /// Invoked, and the scheduler is waiting on it.
    Running,
    /// Ran to completion.
    Succeeded,
    /// In the runnable subgraph, but up to date: same inputs, same
    /// fingerprint as at its last success. Checked, and current.
    SkippedUpToDate,
    /// Outside the runnable subgraph — this run never considered it. A
    /// per-source sync leaves most of the graph here, which is a
    /// different fact from [`RunState::SkippedUpToDate`].
    NotSelected,
    /// An upstream step failed (or was itself blocked); not invoked.
    Blocked,
    Failed,
}

impl RunState {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know — a state file
    /// written by a newer runner, say. The caller decides what an
    /// unrecognized state means rather than being handed a wrong one.
    pub fn parse(s: &str) -> Option<RunState> {
        s.parse().ok()
    }

    /// Whether the step is finished for this run.
    pub const fn is_terminal(self) -> bool {
        !matches!(self, RunState::Running)
    }

    /// Whether it finished without failing. `NotSelected` counts: not
    /// being asked for is not a failure.
    pub const fn is_ok(self) -> bool {
        matches!(
            self,
            RunState::Succeeded | RunState::SkippedUpToDate | RunState::NotSelected
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// strum and serde spell these independently — the state file is
    /// written through `as_str`, the event stream through serde — so a
    /// consumer reading both would otherwise see two spellings of one
    /// state.
    #[test]
    fn as_str_matches_the_serde_spelling() {
        for &v in RunState::VARIANTS {
            let json = serde_json::to_string(&v).unwrap();
            assert_eq!(json, format!("\"{}\"", v.as_str()), "{v:?}");
            assert_eq!(RunState::parse(v.as_str()), Some(v));
        }
    }

    #[test]
    fn an_unknown_spelling_parses_to_none_rather_than_a_guess() {
        assert_eq!(RunState::parse("skipped"), None);
        assert_eq!(RunState::parse(""), None);
    }
}
