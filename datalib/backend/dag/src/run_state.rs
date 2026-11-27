//! How a step ended, as one named vocabulary: an invocation's outcome,
//! or what a request that reached the step and ran nothing of it made of
//! it.
//!
//! These values travel on several surfaces — `Event::InvocationEnd`,
//! `Event::RequestEnd`'s steps, `steps.last_status`, `invocations.outcome`
//! and `request_steps.outcome` in `system/supervisor.sqlite`
//! (`supervisor/record.rs`), an older build's `run_steps.state`, and the
//! HTTP API — and every producer and consumer has to agree on the
//! spelling. Naming them once is what keeps those copies from drifting
//! apart. What a step is doing now is the tick's `StateKind`.

use serde::{Deserialize, Serialize};
use strum::{EnumString, IntoStaticStr, VariantArray};

/// How a step ended: `running` while its invocation is live.
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
    /// Invoked, and the loop is waiting on it.
    Running,
    /// Ran to completion.
    Succeeded,
    /// Wanted, but up to date: same inputs, same fingerprint as at its
    /// last success. Checked, and current.
    SkippedUpToDate,
    /// A step it reads has never published anything; not invoked.
    Blocked,
    Failed,
    /// Told to stop from outside, and stopped: it checkpointed what it
    /// had and exited. Not a failure — nothing went wrong — and not a
    /// success: the work is not done, so it runs again next time, and
    /// its dependents wait for that run rather than reading a partial
    /// result now.
    Stopped,
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

    /// Whether it finished without failing.
    pub const fn is_ok(self) -> bool {
        matches!(self, RunState::Succeeded | RunState::SkippedUpToDate)
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
