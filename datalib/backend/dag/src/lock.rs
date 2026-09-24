//! The runner's claim on a data root: one runner per root, taken as a
//! `flock(2)` the kernel releases when the holder dies (`datalib_flock`).
//!
//! The runner and the server take separate locks on separate files; the
//! crate README says why they cannot share one.

use std::path::Path;
use std::time::{Duration, Instant};

pub use datalib_flock::{FileLock, LockError};

/// The runner's claim, relative to the data root. A sibling of
/// `system/supervisor.sqlite`, whose record is the thing it guards.
pub const RUNNER_LOCK_REL_PATH: &str = "system/runner-lock";

/// How long a starting runner keeps trying a lock someone holds. The
/// server's `runner_is_held` answers by taking the lock for an instant,
/// and it asks on every change under the root — most often just as a
/// run starts — so a runner that gave up at once would sometimes refuse
/// to start against nobody. A real second runner holds it far longer.
const PROBE_GRACE: Duration = Duration::from_secs(2);
const PROBE_POLL: Duration = Duration::from_millis(10);

pub fn acquire_runner(data_root: &Path) -> Result<FileLock, LockError> {
    acquire_runner_within(data_root, PROBE_GRACE)
}

/// One attempt, no grace: for a client that retries on its own schedule.
pub fn try_acquire_runner(data_root: &Path) -> Result<FileLock, LockError> {
    acquire_runner_within(data_root, Duration::ZERO)
}

fn acquire_runner_within(data_root: &Path, grace: Duration) -> Result<FileLock, LockError> {
    let path = data_root.join(RUNNER_LOCK_REL_PATH);
    let deadline = Instant::now() + grace;
    loop {
        match FileLock::acquire(&path) {
            Err(e) if e.is_held() && Instant::now() < deadline => std::thread::sleep(PROBE_POLL),
            claimed => return claimed,
        }
    }
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

    /// A runner that meets a brief holder — the server's probe, which
    /// takes the lock for an instant — must still start. Without the
    /// grace it refused with "another datalib-dag is already running",
    /// and the sync failed with no run recorded (onboarding-pdf.spec.ts
    /// on CI). The probe is held for 50ms here so the collision is
    /// certain rather than a race to hit a microsecond window.
    #[test]
    #[cfg(unix)]
    fn a_runner_starts_through_a_brief_probe() {
        let tmp = tempfile::tempdir().unwrap();
        let probe = acquire_runner(tmp.path()).expect("the probe's claim");
        let released = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(probe);
        });
        acquire_runner(tmp.path()).expect("a runner must outlast a probe");
        released.join().unwrap();
    }

    /// The grace is for probes, not for a second runner: one that stays
    /// held is still refused.
    #[test]
    #[cfg(unix)]
    fn a_held_root_is_still_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let _first = acquire_runner(tmp.path()).expect("first claim");
        let err = acquire_runner_within(tmp.path(), Duration::from_millis(50))
            .expect_err("a second runner must be refused");
        assert!(err.is_held(), "{err}");
    }
}
