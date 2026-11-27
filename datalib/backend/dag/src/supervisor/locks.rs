//! Named locks: what keeps steps that must not run together apart, beyond
//! the sinks they write and read. A lock has `slots`; a step holding it
//! `shared` takes one, and one holding it `exclusive` takes all of them,
//! so `slots = 1` is a mutex and `slots = N` lets N run at once. The
//! config declares them (`[[locks]]`) and a step names the ones it holds
//! (`locks = [...]`); a step that names none holds one default lock
//! ([`default_for`]). The tick does the accounting (`tick.rs`); the words
//! a config uses are `datalib_config::locks`. `dag/README.md` § "What
//! keeps steps apart: locks".

use datalib_config::locks::{Hold, CPU, DEFAULT_LOCKS, NETWORK};
use datalib_config::LockEntry;

use crate::step::StepSpec;

/// A declared lock.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockSpec {
    pub name: String,
    pub slots: usize,
}

/// The locks every config has, whether or not it declares them: the two
/// budgets, as `--parallelism 4` sizes them. A config's `[[locks]]` entry
/// of the same name resizes one; `--parallelism N` sets both to N, over
/// the config. Any other lock a step holds, the config declares.
pub fn defaults() -> Vec<LockSpec> {
    DEFAULT_LOCKS
        .into_iter()
        .map(|name| LockSpec {
            name: name.to_string(),
            slots: 4,
        })
        .collect()
}

/// Every lock a step may hold: the defaults, resized or joined by what the
/// config declares.
pub fn declared(locks: &[LockEntry]) -> Vec<LockSpec> {
    let mut specs = defaults();
    for l in locks {
        match specs.iter_mut().find(|s| s.name == l.name) {
            Some(default) => default.slots = l.slots,
            None => specs.push(LockSpec {
                name: l.name.clone(),
                slots: l.slots,
            }),
        }
    }
    specs
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
