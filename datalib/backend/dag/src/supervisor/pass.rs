//! A pass: the steps one verb is asked of, one at a time, and how each
//! stands. The launch's migrate pass and a wipe that frees disk are both
//! one (`dag/README.md` §"Upgrading a root", §"Resets and purges"), and
//! the app draws either the same way.

use serde::{Deserialize, Serialize};
use strum::{EnumString, IntoStaticStr, VariantArray};

use crate::step::StoreBytes;

/// How a pass stands with one step.
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
pub enum PassState {
    Waiting,
    Running,
    Done,
    Failed,
}

impl PassState {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PassRow {
    pub step: String,
    pub state: PassState,
    pub error: Option<String>,
    /// What a step that shrank its store reported: its size before and
    /// after.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub store_bytes: Option<StoreBytes>,
}

impl PassRow {
    pub fn waiting(step: &str) -> Self {
        PassRow {
            step: step.to_string(),
            state: PassState::Waiting,
            error: None,
            store_bytes: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strum_and_serde_agree_on_a_pass_state() {
        for state in PassState::VARIANTS {
            let word = serde_json::to_value(state).unwrap();
            assert_eq!(word, state.as_str());
            assert_eq!(PassState::parse(state.as_str()), Some(*state));
        }
        assert_eq!(PassState::parse("a state from a newer build"), None);
    }
}
