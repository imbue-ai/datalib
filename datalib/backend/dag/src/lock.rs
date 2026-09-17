//! The runner's claim on a data root: one runner per root, taken as a
//! `flock(2)` the kernel releases when the holder dies (`datalib_flock`).
//!
//! The runner and the server take separate locks on separate files; the
//! crate README says why they cannot share one.

use std::path::Path;

pub use datalib_flock::{FileLock, LockError};

/// The runner's claim, relative to the data root. A sibling of
/// `system/dag_state.json`, which is the thing it guards.
pub const RUNNER_LOCK_REL_PATH: &str = "system/runner-lock";

pub fn acquire_runner(data_root: &Path) -> Result<FileLock, LockError> {
    FileLock::acquire(&data_root.join(RUNNER_LOCK_REL_PATH))
}

/// Is a runner holding this root right now? Read-only and racy, as
/// [`FileLock::is_held`] says; a probe that cannot answer says "held".
pub fn runner_is_held(data_root: &Path) -> bool {
    FileLock::is_held(&data_root.join(RUNNER_LOCK_REL_PATH))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The runner's lock and the server's are different files, so a
    /// server holding its own claim does not lock out the runner it is
    /// about to spawn.
    #[test]
    #[cfg(unix)]
    fn the_runner_and_server_claims_do_not_collide() {
        let tmp = tempfile::tempdir().unwrap();
        let _server = FileLock::acquire(&tmp.path().join("system/lock")).expect("server claim");
        acquire_runner(tmp.path()).expect("runner claim must not contend with it");
    }
}
