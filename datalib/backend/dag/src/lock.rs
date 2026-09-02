//! An exclusive claim on a file, held for the life of a process.
//!
//! Two invariants in this tree need one, and they are not the same
//! claim:
//!
//!   * **One runner per data root.** The scheduler's state is a single
//!     JSON file it rewrites after every terminal step, and the steps
//!     it spawns write raw stores whose doltlite working set is shared
//!     across processes. Two runners on one root interleave both.
//!     That is [`RUNNER_LOCK_REL_PATH`], taken by `datalib-dag`.
//!   * **One server per data root**, which `datalib-http` takes on its
//!     own file for its own reasons (the API token, the job and
//!     feedback stores). It has to be a *different* file: the server
//!     spawns the runner, so sharing one lock would deadlock the server
//!     against its own child.
//!
//! `flock(2)` rather than a pid file, because the kernel releases it
//! when the holder dies — a crashed process leaves no stale lock to
//! reason about, which is the failure mode pid files are famous for.
//! The file's *contents* are advisory: they exist so a refusal can name
//! the holder, and are never trusted to decide whether it is held.
//!
//! This lives in the runner's crate rather than the server's because
//! the runner is the one with an invariant it cannot state any other
//! way, and because `datalib-http` already depends on this crate —
//! so one implementation serves both rather than two spellings of
//! `flock` drifting apart.

use std::fmt;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

/// The runner's claim, relative to the data root. A sibling of
/// `system/dag_state.json`, which is the thing it guards.
pub const RUNNER_LOCK_REL_PATH: &str = "system/runner-lock";

/// Why a lock could not be taken.
#[derive(Debug)]
pub enum LockError {
    /// Another process holds it. `holder` is whatever that process
    /// wrote about itself, when it wrote anything.
    Held {
        path: PathBuf,
        holder: Option<String>,
    },
    /// The lock file itself could not be opened or locked.
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}

impl LockError {
    /// The holder's own description, when it left one.
    pub fn holder(&self) -> Option<&str> {
        match self {
            LockError::Held { holder, .. } => holder.as_deref(),
            LockError::Io { .. } => None,
        }
    }

    pub fn path(&self) -> &Path {
        match self {
            LockError::Held { path, .. } | LockError::Io { path, .. } => path,
        }
    }

    /// True when someone else holds it, as opposed to the lock file
    /// being unusable.
    pub fn is_held(&self) -> bool {
        matches!(self, LockError::Held { .. })
    }
}

impl fmt::Display for LockError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LockError::Held { path, holder } => write!(
                f,
                "another process already holds {}{}",
                path.display(),
                match holder {
                    Some(h) => format!(" — {h}"),
                    None => String::new(),
                }
            ),
            LockError::Io { path, source } => {
                write!(f, "could not take the lock {}: {source}", path.display())
            }
        }
    }
}

impl std::error::Error for LockError {}

/// An exclusive claim, released when this value drops — or when the
/// process exits for any reason, including a crash.
#[derive(Debug)]
pub struct FileLock {
    file: File,
    path: PathBuf,
}

impl FileLock {
    /// Take the lock at `path`, creating the file and its parent if
    /// needed, or fail saying who holds it.
    pub fn acquire(path: &Path) -> Result<Self, LockError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| LockError::Io {
                path: dir.to_path_buf(),
                source: e,
            })?;
        }
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(path)
            .map_err(|e| LockError::Io {
                path: path.to_path_buf(),
                source: e,
            })?;

        take(&file).map_err(|e| {
            if e.kind() == std::io::ErrorKind::WouldBlock {
                // Read what the holder said about itself. Best effort:
                // the lock is the truth, this is only for the message.
                let holder = std::fs::read_to_string(path)
                    .ok()
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty());
                LockError::Held {
                    path: path.to_path_buf(),
                    holder,
                }
            } else {
                LockError::Io {
                    path: path.to_path_buf(),
                    source: e,
                }
            }
        })?;

        let mut lock = Self {
            file,
            path: path.to_path_buf(),
        };
        lock.describe(&format!("held by pid {}", std::process::id()));
        Ok(lock)
    }

    /// The runner's claim on a data root.
    pub fn acquire_runner(data_root: &Path) -> Result<Self, LockError> {
        Self::acquire(&data_root.join(RUNNER_LOCK_REL_PATH))
    }

    /// Is this lock held by some other process right now?
    ///
    /// A **read-only** probe, and that is the whole reason it exists
    /// separately from [`Self::acquire`]: acquiring creates the file if
    /// absent and, on success, rewrites its contents with a holder
    /// line. Both are right for a process claiming the root and wrong
    /// for one merely asking — a caller on a timer would rewrite the
    /// file every few seconds, and a root that had never run would
    /// sprout a lock file from being looked at.
    ///
    /// A missing file means nobody has ever taken it, which is "not
    /// held". So is a file we cannot open: an unreadable lock tells us
    /// nothing, and claiming a run is in flight on that basis would be
    /// a guess.
    ///
    /// Racy by nature — the holder may let go a microsecond later, and
    /// the momentary claim this makes can refuse a runner starting in
    /// the same instant. Both are only acceptable where being one poll
    /// stale costs nothing; don't build an invariant on it.
    pub fn is_held(path: &Path) -> bool {
        let Ok(file) = File::open(path) else {
            return false;
        };
        match take(&file) {
            Ok(()) => {
                release(&file);
                false
            }
            Err(e) => e.kind() == std::io::ErrorKind::WouldBlock,
        }
    }

    /// Is a `datalib-dag` run holding this data root right now?
    pub fn runner_is_held(data_root: &Path) -> bool {
        Self::is_held(&data_root.join(RUNNER_LOCK_REL_PATH))
    }

    /// Replace what this lock says about its holder, so a later
    /// would-be owner's refusal can point at something useful.
    /// Failure to write is not worth failing over.
    pub fn describe(&mut self, what: &str) {
        // Truncate-and-write rather than append: this file holds one
        // fact, and a partial old line under a new one would read as
        // two holders.
        let _ = self.file.set_len(0);
        let _ = std::io::Seek::seek(&mut self.file, std::io::SeekFrom::Start(0));
        let _ = writeln!(self.file, "{what}");
        let _ = self.file.flush();
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(unix)]
fn take(file: &File) -> std::io::Result<()> {
    use std::os::unix::io::AsRawFd;
    // LOCK_NB: refuse immediately rather than blocking behind a process
    // someone forgot about.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Drop a claim taken by [`take`] without closing the file. Only
/// [`FileLock::is_held`] needs this — every other holder releases by
/// dropping the `File`.
#[cfg(unix)]
fn release(file: &File) {
    use std::os::unix::io::AsRawFd;
    unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
}

#[cfg(not(unix))]
fn release(_file: &File) {}

/// No advisory-lock call on this platform, so the claim succeeds
/// unconditionally. The shipped targets are macOS and Linux; leaving
/// this permissive keeps a Windows build compiling rather than
/// pretending to a guarantee it doesn't have.
#[cfg(not(unix))]
fn take(_file: &File) -> std::io::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The read-only probe answers the same question `acquire` does,
    /// without the two side effects that make `acquire` wrong on a
    /// timer: it creates no file, and it rewrites no holder line.
    #[test]
    #[cfg(unix)]
    fn is_held_answers_without_touching_the_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join(RUNNER_LOCK_REL_PATH);

        // Never taken: not held, and asking must not conjure the file.
        assert!(!FileLock::runner_is_held(tmp.path()));
        assert!(!path.exists(), "a probe created {}", path.display());

        let mut held = FileLock::acquire_runner(tmp.path()).expect("claim");
        held.describe("running a sync since 10:04");
        assert!(FileLock::runner_is_held(tmp.path()));
        // …and the holder's own line survives being asked about, which
        // `acquire` would have overwritten on success.
        assert_eq!(
            std::fs::read_to_string(&path).unwrap().trim(),
            "running a sync since 10:04"
        );

        drop(held);
        assert!(!FileLock::runner_is_held(tmp.path()));
        // Probing an existing-but-free lock must leave it takeable.
        assert!(FileLock::acquire_runner(tmp.path()).is_ok());
    }

    #[test]
    #[cfg(unix)]
    fn a_second_claim_is_refused_and_names_the_holder() {
        let tmp = tempfile::tempdir().unwrap();
        let mut first = FileLock::acquire_runner(tmp.path()).expect("first claim");
        first.describe("running a sync since 10:04");

        let err = FileLock::acquire_runner(tmp.path()).expect_err("second claim must fail");
        assert!(err.is_held());
        assert_eq!(err.holder(), Some("running a sync since 10:04"));
    }

    /// Releasing has to actually release, or a finished run would leave
    /// the root unusable until a reboot.
    #[test]
    #[cfg(unix)]
    fn dropping_releases() {
        let tmp = tempfile::tempdir().unwrap();
        let first = FileLock::acquire_runner(tmp.path()).expect("first claim");
        drop(first);
        FileLock::acquire_runner(tmp.path()).expect("claim after release");
    }

    /// The runner's lock and the server's are different files, so a
    /// server holding its own claim does not lock out the runner it is
    /// about to spawn.
    #[test]
    #[cfg(unix)]
    fn the_runner_and_server_claims_do_not_collide() {
        let tmp = tempfile::tempdir().unwrap();
        let _server = FileLock::acquire(&tmp.path().join("system/lock")).expect("server claim");
        FileLock::acquire_runner(tmp.path()).expect("runner claim must not contend with it");
    }
}
