//! Named locks: what keeps steps that must not run together apart, beyond
//! the sinks they write and read. A lock has `slots`; a step holding it
//! `shared` takes one, and one holding it `exclusive` takes all of them,
//! so `slots = 1` is a mutex and `slots = N` lets N run at once. The
//! config declares them (`[[locks]]`) and a step names the ones it holds
//! (`locks = [...]`); a step that names none holds one default lock
//! ([`default_for`]). The tick does the accounting (`tick.rs`); this is
//! the vocabulary. `dag/README.md` § "What keeps steps apart: locks".

use serde::{Deserialize, Serialize};
use strum::{EnumString, IntoStaticStr, VariantArray};

use crate::step::StepSpec;

/// How a step holds a lock.
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
pub enum Hold {
    /// One slot.
    Shared,
    /// Every slot: nobody else holds it at the same time.
    Exclusive,
}

impl Hold {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }
}

/// A declared lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockSpec {
    pub name: String,
    pub slots: usize,
}

/// The budget a step that declares no inputs holds when it names no lock:
/// in practice a download, so a rate limit on one keeps no render from
/// starting.
pub const NETWORK: &str = "network";
/// The budget a step that reads something holds when it names no lock.
pub const CPU: &str = "cpu";

/// The locks every config has, whether or not it declares them: the two
/// budgets, as `--parallelism 4` sizes them. A config's `[[locks]]` entry
/// of the same name resizes one; `--parallelism N` sets both to N, over
/// the config. Any other lock a step holds, the config declares.
pub fn defaults() -> Vec<LockSpec> {
    [(NETWORK, 4), (CPU, 4)]
        .into_iter()
        .map(|(name, slots)| LockSpec {
            name: name.to_string(),
            slots,
        })
        .collect()
}

/// The lock a step holds when it names none: `network` for a step that
/// declares no inputs, `cpu` for any other, a fan-in whose patterns match
/// nothing yet among them.
pub fn default_for(spec: &StepSpec) -> &'static str {
    if spec.is_source() {
        NETWORK
    } else {
        CPU
    }
}

/// The locks a step holds, by name.
pub fn held_by(spec: &StepSpec) -> Vec<(String, Hold)> {
    match &spec.locks {
        Some(locks) => locks.clone(),
        None => vec![(default_for(spec).to_string(), Hold::Shared)],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// strum and serde spell these independently; the config is read with
    /// serde and the record's detail with strum.
    #[test]
    fn hold_as_str_matches_the_serde_spelling() {
        for &v in Hold::VARIANTS {
            let json = serde_json::to_string(&v).unwrap();
            assert_eq!(json, format!("\"{}\"", v.as_str()), "{v:?}");
            assert_eq!(Hold::parse(v.as_str()), Some(v));
        }
    }
}
