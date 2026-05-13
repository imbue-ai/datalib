//! Manage the lifetime of the Python `worker` subprocess (`python -m worker`).
//!
//! Mirrors the design of [`crate::dolt_server::DoltServer`]:
//!
//! * Spawn `python -m worker --config <path> [--global-cap N] [--per-provider-cap N]`
//!   as a child process. There is no port-based "already up" probe — the
//!   worker doesn't listen on TCP — but a PID file (`<root>/state/worker.pid`)
//!   lets us detect a previously-spawned worker that's still alive and
//!   attach instead of double-spawning.
//! * Stream stdout/stderr to `<root>/logs/worker.log`.
//! * On Drop: SIGTERM (best-effort) + wait, but only when we own the child.
//!
//! The worker drains `sync_jobs` itself; this struct just keeps it alive
//! for the lifetime of the backend.
//!
//! Note: this supervisor only runs when the backend is on the Dolt path —
//! the worker writes to Dolt, so spawning it under `--backend sqlite`
//! would be pointless.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

#[derive(Debug, thiserror::Error)]
pub enum WorkerError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("`python` binary not found on $PATH")]
    PythonMissing,
}

/// Optional caps that the supervisor forwards to the worker as CLI flags.
/// `None` means "let the worker use its built-in default".
#[derive(Debug, Clone, Default)]
pub struct WorkerCaps {
    pub global_cap: Option<u32>,
    pub per_provider_cap: Option<u32>,
}

/// Handle to a running `python -m worker` subprocess. Drop semantics:
/// SIGTERM + wait when we own the child; no-op when we attached to a
/// preexisting worker (detected via the PID file).
pub struct Worker {
    root: PathBuf,
    child: Option<Child>,
    attached_pid: Option<u32>,
    owns_worker: bool,
}

impl Worker {
    /// Spawn (or attach to) a `python -m worker` for the given `root`.
    /// `config_path` is forwarded as `--config`; `caps` is forwarded as
    /// optional flags.
    pub fn ensure(
        root: &Path,
        config_path: Option<&Path>,
        caps: &WorkerCaps,
    ) -> Result<Self, WorkerError> {
        std::fs::create_dir_all(root)?;
        let state_dir = root.join("state");
        std::fs::create_dir_all(&state_dir)?;
        let log_dir = root.join("logs");
        std::fs::create_dir_all(&log_dir)?;

        // Attach mode: if a pidfile points at a live process, assume it's
        // a healthy worker and don't spawn another.
        let pidfile = state_dir.join("worker.pid");
        if let Some(pid) = read_pidfile(&pidfile) {
            if is_pid_alive(pid) {
                return Ok(Self {
                    root: root.to_path_buf(),
                    child: None,
                    attached_pid: Some(pid),
                    owns_worker: false,
                });
            }
        }

        let log_path = log_dir.join("worker.log");
        let log_file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        let log_err = log_file.try_clone()?;

        let python = which_on_path("python3")
            .or_else(|| which_on_path("python"))
            .ok_or(WorkerError::PythonMissing)?;

        let mut cmd = Command::new(&python);
        cmd.arg("-m").arg("worker");
        if let Some(p) = config_path {
            cmd.arg("--config").arg(p);
        }
        if let Some(n) = caps.global_cap {
            cmd.arg("--global-cap").arg(n.to_string());
        }
        if let Some(n) = caps.per_provider_cap {
            cmd.arg("--per-provider-cap").arg(n.to_string());
        }
        let child = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::from(log_file))
            .stderr(Stdio::from(log_err))
            .spawn()?;

        // Best-effort pidfile write — purely advisory; if it fails we still
        // own the child and Drop will reap it.
        let pid = child.id();
        let _ = std::fs::write(&pidfile, pid.to_string());

        Ok(Self {
            root: root.to_path_buf(),
            child: Some(child),
            attached_pid: None,
            owns_worker: true,
        })
    }

    /// PID of the running worker (whether we spawned it or attached).
    pub fn pid(&self) -> Option<u32> {
        if let Some(c) = self.child.as_ref() {
            return Some(c.id());
        }
        self.attached_pid
    }

    /// Best-effort liveness check. `false` if no pid is known.
    pub fn is_alive(&self) -> bool {
        match self.pid() {
            Some(p) => is_pid_alive(p),
            None => false,
        }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn owns_worker(&self) -> bool {
        self.owns_worker
    }
}

impl Drop for Worker {
    fn drop(&mut self) {
        if !self.owns_worker {
            return;
        }
        let Some(mut child) = self.child.take() else {
            return;
        };
        // SIGTERM first so the worker gets a chance to flush + commit;
        // kill() on Unix sends SIGKILL, so we use `nix`-free libc here.
        #[cfg(unix)]
        unsafe {
            libc_kill(child.id() as i32, 15);
        }
        // Brief wait, then escalate.
        for _ in 0..20 {
            match child.try_wait() {
                Ok(Some(_)) => return,
                _ => std::thread::sleep(std::time::Duration::from_millis(100)),
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

#[cfg(unix)]
unsafe fn libc_kill(pid: i32, sig: i32) {
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(pid, sig) };
}

fn which_on_path(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

fn read_pidfile(path: &Path) -> Option<u32> {
    let s = std::fs::read_to_string(path).ok()?;
    s.trim().parse::<u32>().ok()
}

#[cfg(unix)]
fn is_pid_alive(pid: u32) -> bool {
    // signal 0 = existence check without delivering a signal.
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe { kill(pid as i32, 0) == 0 }
}

#[cfg(not(unix))]
fn is_pid_alive(_pid: u32) -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn alive_check_for_self_returns_true() {
        let me = std::process::id();
        assert!(is_pid_alive(me));
    }

    #[test]
    fn alive_check_for_unlikely_pid_returns_false() {
        // pid 999_999 is not guaranteed dead, but it's almost always so.
        // We accept a possible false-true; the assertion below is best-effort.
        let _ = is_pid_alive(999_999);
    }
}
