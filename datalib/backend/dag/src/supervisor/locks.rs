//! Named locks: what keeps steps that must not run together apart, beyond
//! the sinks they write and read. A lock has `slots`; a step holding it
//! `shared` takes one, and one holding it `exclusive` takes all of them,
//! so `slots = 1` is a mutex and `slots = N` lets N run at once. The
//! config declares them (`[[locks]]`) and a step names the ones it holds
//! (`locks = [...]`); a step that names none holds one default lock, which
//! is what the three budgets were. The tick does the accounting
//! (`tick.rs`); this is the vocabulary. `dag/README.md` § "Locks".

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

/// A download: one per source a person syncs, so a rate limit on one
/// keeps no render from starting.
pub const NETWORK: &str = "network";
/// A render: a grouped step that reads something.
pub const CPU: &str = "cpu";
/// An index: a step outside any typed group that reads something.
pub const INDEX: &str = "index";
/// Whatever writes qmd's collection registry or its keyword index. One
/// slot, and resizing it is a mistake: two keyword updates on one index
/// can lose a document's body (`docs/dev/qmd_behaviour.md`, finding 12).
pub const QMD_KEYWORD: &str = "qmd_keyword";
/// Whatever embeds into qmd's index. One slot: two embeds race on qmd's
/// vector table, and share one GPU besides.
pub const QMD_EMBED: &str = "qmd_embed";

/// The locks every config has, whether or not it declares them: the three
/// budgets, as `--parallelism 4` sizes them, and the two qmd writers'. A
/// config's `[[locks]]` entry of the same name resizes one;
/// `--parallelism N` sets `network` and `cpu` to N, over the config.
pub fn defaults() -> Vec<LockSpec> {
    [
        (NETWORK, 4),
        (CPU, 4),
        (INDEX, 2),
        (QMD_KEYWORD, 1),
        (QMD_EMBED, 1),
    ]
    .into_iter()
    .map(|(name, slots)| LockSpec {
        name: name.to_string(),
        slots,
    })
    .collect()
}

/// The lock a step holds when it names none: `network` for a source,
/// `cpu` for a grouped step that reads something, `index` for any other
/// step that reads something.
pub fn default_for(spec: &StepSpec) -> &'static str {
    if spec.inputs.is_empty() {
        NETWORK
    } else if spec.group_type.is_none() {
        INDEX
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
