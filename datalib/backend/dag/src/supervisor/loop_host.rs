//! What the loop asks of the host that runs it, for a loop that never
//! exits: the environment a step it starts gets, and whether the loop has
//! work open. Everything else it says, it writes to the stores.

use std::collections::BTreeMap;

/// How [`Runner::run_forever`] (`crate::scheduler::Runner`) asks its host.
pub trait LoopHost: Send + Sync {
    /// What a step started now gets on top of the host's own environment,
    /// built from the config as it is now; or why nothing can run, which
    /// fails the requests open when the loop takes on work. Each
    /// invocation's id and "now" go on top.
    fn step_env(&self) -> Result<BTreeMap<String, String>, String>;
    /// The loop has taken on work, or has none left and nothing runs.
    fn busy(&self, busy: bool);
}
