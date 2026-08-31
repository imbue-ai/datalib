//! One writer per data root, enforced.
//!
//! `system/` is this server's own state: the feedback and job stores,
//! the API token, the job logs, and — through the sync worker —
//! `system/dag_state.json` and every raw store the runner it spawns
//! writes into. All of that assumes a single owner.
//!
//! Two `datalib-http` processes on one data root break that quietly
//! rather than loudly:
//!
//!   * **doltlite's working set is per file and shared across
//!     processes** (see AGENTS.md). Two writers on
//!     `system/jobs.doltlite_db` commit each other's in-flight rows —
//!     the same failure that moved feedback into its own file.
//!   * **The API token is published, not negotiated.** The second
//!     server overwrites `system/api-token` with its own, so anything
//!     reading the file — an agent following `/agent/config.md`, a
//!     script — now authenticates against a server that isn't the one
//!     the user is looking at.
//!   * **Two sync workers** can run `datalib-dag` concurrently on one
//!     root, and the scheduler's persisted state is a single JSON file.
//!
//! None of those announces itself. The lock turns all three into one
//! refusal at startup, naming the process that already holds the root.
//!
//! `flock(2)` rather than a pid file, because the kernel releases it
//! when the holder dies — a crashed server leaves no stale lock to
//! reason about, which is the failure mode pid files are famous for.
//! The file's *contents* are advisory: they exist so the refusal can
//! say where the other server is listening, and are never trusted to
//! decide whether the lock is held.
//!
//! The mechanism itself lives in [`datalib_dag::lock`], because the
//! runner needs the same thing for its own invariant (one runner per
//! root) and this crate already depends on that one. What stays here is
//! the part that is about *servers*: which file, and what the refusal
//! says. The two locks are deliberately different files — this process
//! spawns the runner, so sharing one would deadlock the server against
//! its own child.

use std::path::{Path, PathBuf};

use datalib_dag::lock::FileLock;

/// Why the data root could not be claimed.
#[derive(Debug)]
pub enum LockError {
    /// Another server holds it. `holder` is whatever that process wrote
    /// about itself, when it wrote anything.
    Held {
        path: PathBuf,
        holder: Option<String>,
    },
    /// The lock file itself could not be opened or locked.
    Io { path: PathBuf, source: String },
}

impl std::fmt::Display for LockError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LockError::Held { path, holder } => {
                write!(
                    f,
                    "another datalib-http already owns this data root{}.\n\
                     Only one server may write a root's system/ directory: they would \
                     overwrite each other's job and feedback stores and each other's API \
                     token. Stop the other one, or point this server at a different root.\n\
                     (lock: {})",
                    match holder {
                        Some(h) => format!(" — {h}"),
                        None => String::new(),
                    },
                    path.display()
                )
            }
            LockError::Io { path, source } => {
                write!(
                    f,
                    "could not take the data-root lock {}: {source}",
                    path.display()
                )
            }
        }
    }
}

impl std::error::Error for LockError {}

/// An exclusive claim on a data root, held for as long as the value
/// lives. Dropping it — or the process exiting for any reason,
/// including a crash — releases the lock.
#[derive(Debug)]
pub struct DataRootLock(FileLock);

impl DataRootLock {
    /// Claim `root` exclusively, or fail saying who holds it.
    ///
    /// Call this before anything writes under `system/` — in
    /// particular before the API token is minted, so a refused server
    /// never clobbers the running one's token on its way out.
    pub fn acquire(root: &Path) -> Result<Self, LockError> {
        FileLock::acquire(&datalib_core::layout::lock_file(root))
            .map(Self)
            .map_err(|e| {
                let path = e.path().to_path_buf();
                if e.is_held() {
                    LockError::Held {
                        path,
                        holder: e.holder().map(str::to_string),
                    }
                } else {
                    LockError::Io {
                        path,
                        source: e.to_string(),
                    }
                }
            })
    }

    /// Record where this server can be reached, so a later would-be
    /// owner's refusal can point at it. Called once the listener has a
    /// port; failure to write is not worth failing a boot over.
    pub fn announce(&mut self, base_url: &str) {
        self.0.describe(&format!(
            "served at {base_url} by pid {}",
            std::process::id()
        ));
    }

    pub fn path(&self) -> &Path {
        self.0.path()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point: the second server is refused, and told where
    /// the first one is.
    #[test]
    #[cfg(unix)]
    fn a_second_claim_on_one_root_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let mut first = DataRootLock::acquire(tmp.path()).expect("first claim");
        first.announce("http://127.0.0.1:8731");

        let err = DataRootLock::acquire(tmp.path()).expect_err("second claim must fail");
        let msg = err.to_string();
        assert!(msg.contains("already owns this data root"), "{msg}");
        assert!(
            msg.contains("http://127.0.0.1:8731"),
            "the refusal must name where the holder is listening: {msg}"
        );
    }

    /// Releasing has to actually release — otherwise restarting a
    /// server would need a reboot.
    #[test]
    #[cfg(unix)]
    fn dropping_the_lock_frees_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let first = DataRootLock::acquire(tmp.path()).unwrap();
        drop(first);
        DataRootLock::acquire(tmp.path()).expect("a released root must be claimable");
    }

    /// Different roots are independent; one server per root is the
    /// rule, not one server per machine.
    #[test]
    #[cfg(unix)]
    fn two_roots_do_not_contend() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let _lock_a = DataRootLock::acquire(a.path()).unwrap();
        DataRootLock::acquire(b.path()).expect("a different root is a different lock");
    }

    /// The lock lives under `system/`, which may not exist yet on a
    /// fresh root — claiming one must create it rather than fail.
    #[test]
    fn a_fresh_root_can_be_claimed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("brand-new");
        std::fs::create_dir_all(&root).unwrap();
        let lock = DataRootLock::acquire(&root).expect("fresh root");
        assert!(lock.path().exists());
    }
}
