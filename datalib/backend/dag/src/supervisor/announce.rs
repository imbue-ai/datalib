//! How a process waiting on the loop's store hears that something changed:
//! whoever commits says so, as a step says it sealed. `Store` announces
//! after every commit it makes (`request opened <id>`, `record saved`, …),
//! and so do the two writers that are not the store: the server when the
//! config changes, and a process letting `runner-lock` go.
//!
//! A listener is a FIFO in `system/supervisor-listeners/`; an announcement
//! is one line written to each, under `PIPE_BUF` so lines never interleave.
//! A FIFO nobody has open is a dead process's, and announcing removes it.
//! A FIFO's path, unlike a Unix socket's, has no length limit.
//!
//! A commit nobody announced — a `sqlite3` shell, an older build — is found
//! by the listener's backstop, which looks at the store every
//! [`BACKSTOP`] and says so at ERROR.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::store::Store;

/// How long a listener waits for an announcement before looking at the
/// store anyway.
pub const BACKSTOP: Duration = Duration::from_secs(5);

static MISSED: AtomicU64 = AtomicU64::new(0);

/// How many times, in this process, a backstop found a commit nobody
/// announced. In our own code any at all is a bug; tests assert none.
pub fn missed_announcements() -> u64 {
    MISSED.load(Ordering::Relaxed)
}

pub fn listeners_dir(data_root: &Path) -> PathBuf {
    datalib_runtime::layout::supervisor_db(data_root).with_file_name("supervisor-listeners")
}

/// Tell every listener `what`. Never fails: a listener that misses it
/// still has its backstop, which says so.
pub fn announce(dir: &Path, what: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let line = format!("{what}\n");
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "fifo") {
            imp::tell(&path, line.as_bytes());
        }
    }
}

/// Wakes at an announcement, or after [`BACKSTOP`] with none.
pub struct Listener {
    fifo: Option<imp::Fifo>,
    /// Who is listening, for the ERROR line.
    who: &'static str,
    backstop: Duration,
    heard: bool,
    checked: Option<i64>,
}

impl Listener {
    pub async fn new(store: &Store, who: &'static str) -> Listener {
        let fifo = match imp::Fifo::make(store.listeners()) {
            Ok(fifo) => Some(fifo),
            Err(e) => {
                tracing::error!("{who}: cannot listen, so only its backstop wakes it: {e:#}");
                None
            }
        };
        Listener {
            fifo,
            who,
            backstop: BACKSTOP,
            heard: false,
            checked: store.data_version().await.ok(),
        }
    }

    pub fn backstop(mut self, after: Duration) -> Listener {
        self.backstop = after;
        self
    }

    /// The announcements made since the last call, waiting for one if
    /// there are none; empty when the backstop woke it instead. Safe to
    /// drop mid-wait: an announcement is used up only by the call that
    /// returns it.
    pub async fn next(&mut self, store: &Store) -> Vec<String> {
        // Everything committed up to what was just heard was announced;
        // only what lands after this is the backstop's to judge.
        if self.heard {
            self.checked = store.data_version().await.ok();
            self.heard = false;
        }
        let backstop = tokio::time::sleep(self.backstop);
        tokio::select! {
            biased;
            heard = heard(&mut self.fifo) => {
                self.heard = true;
                heard
            }
            _ = backstop => {
                let now = store.data_version().await.ok();
                let moved = self.checked.is_some() && now.is_some() && now != self.checked;
                let late = self.fifo.as_mut().map(imp::Fifo::drain).unwrap_or_default();
                if moved && late.is_empty() {
                    MISSED.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(
                        "{}: the loop's store changed and nobody announced it; found by the \
                         {:?} backstop, so whatever was written waited that long",
                        self.who,
                        self.backstop
                    );
                }
                self.checked = now;
                late
            }
        }
    }
}

async fn heard(fifo: &mut Option<imp::Fifo>) -> Vec<String> {
    match fifo {
        Some(fifo) => fifo.heard().await,
        None => std::future::pending().await,
    }
}

#[cfg(unix)]
mod imp {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use anyhow::{Context, Result};
    use tokio::net::unix::pipe;

    pub fn tell(path: &Path, line: &[u8]) {
        let opened = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path);
        match opened {
            // A full pipe has plenty to wake its reader already.
            Ok(mut fifo) => {
                let _ = fifo.write(line);
            }
            // No reader: its process is gone.
            Err(e) if e.raw_os_error() == Some(libc::ENXIO) => {
                let _ = std::fs::remove_file(path);
            }
            Err(_) => {}
        }
    }

    /// One listener's FIFO. Dropping it takes the FIFO away.
    pub struct Fifo {
        path: PathBuf,
        rx: pipe::Receiver,
        /// Held so the FIFO always has a writer: with none, a read sees
        /// end-of-file at once instead of waiting.
        _writer: pipe::Sender,
        partial: Vec<u8>,
    }

    /// Tests in one binary share a pid.
    static NEXT: AtomicU64 = AtomicU64::new(0);

    impl Fifo {
        /// Must be called inside a tokio runtime.
        pub fn make(dir: &Path) -> Result<Fifo> {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = dir.join(format!("{}-{n}.fifo", std::process::id()));
            let _ = std::fs::remove_file(&path);
            let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
                .context("a listener's path holds a NUL")?;
            // SAFETY: a valid NUL-terminated path, and a mode.
            if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("mkfifo {}", path.display()));
            }
            let rx = pipe::OpenOptions::new()
                .open_receiver(&path)
                .with_context(|| format!("open {} to listen", path.display()))?;
            let writer = pipe::OpenOptions::new()
                .open_sender(&path)
                .with_context(|| format!("open {} to hold", path.display()))?;
            Ok(Fifo {
                path,
                rx,
                _writer: writer,
                partial: Vec::new(),
            })
        }

        pub async fn heard(&mut self) -> Vec<String> {
            loop {
                if self.rx.readable().await.is_err() {
                    return std::future::pending().await;
                }
                let lines = self.drain();
                if !lines.is_empty() {
                    return lines;
                }
            }
        }

        /// Every whole line waiting, without waiting for more.
        pub fn drain(&mut self) -> Vec<String> {
            let mut buf = [0u8; 4096];
            while let Ok(n) = self.rx.try_read(&mut buf) {
                if n == 0 {
                    break;
                }
                self.partial.extend_from_slice(&buf[..n]);
            }
            let mut lines = Vec::new();
            while let Some(at) = self.partial.iter().position(|&b| b == b'\n') {
                let line: Vec<u8> = self.partial.drain(..=at).collect();
                lines.push(String::from_utf8_lossy(&line[..line.len() - 1]).into_owned());
            }
            lines
        }
    }

    impl Drop for Fifo {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(not(unix))]
mod imp {
    use std::path::Path;

    pub fn tell(_: &Path, _: &[u8]) {}

    pub struct Fifo;

    impl Fifo {
        pub fn make(_: &Path) -> anyhow::Result<Fifo> {
            anyhow::bail!("no FIFOs on this platform")
        }

        pub async fn heard(&mut self) -> Vec<String> {
            std::future::pending().await
        }

        pub fn drain(&mut self) -> Vec<String> {
            Vec::new()
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// The loop's whole cue: another process's commit reaches every
    /// listener, saying what it was, and one made before a listener waits
    /// is not lost.
    #[tokio::test]
    async fn a_commit_is_announced_to_every_listener_and_waits_for_one_not_yet_waiting() {
        let td = tempfile::tempdir().unwrap();
        let store = Store::open(td.path()).await.unwrap();
        let other = Store::open(td.path()).await.unwrap();
        let mut a = Listener::new(&store, "a").await;
        let mut b = Listener::new(&store, "b").await;
        other.pause("x/y", "test").await.unwrap();
        assert_eq!(a.next(&store).await, ["paused x/y"]);
        assert_eq!(b.next(&store).await, ["paused x/y"]);
    }

    /// A listener that died without taking its FIFO away would otherwise
    /// cost every announcement an open that goes nowhere, forever.
    #[tokio::test]
    async fn announcing_removes_a_fifo_nobody_listens_on() {
        let td = tempfile::tempdir().unwrap();
        let dir = listeners_dir(td.path());
        std::fs::create_dir_all(&dir).unwrap();
        let orphan = dir.join("0-0.fifo");
        let c = std::ffi::CString::new(orphan.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid path and a mode.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        announce(&dir, "anything");
        assert!(!orphan.exists());
    }

    /// The backstop's whole job: a commit nobody announced is found, and
    /// counted as the bug it is; one that was announced is not.
    #[tokio::test]
    async fn a_commit_nobody_announced_is_found_by_the_backstop_and_counted() {
        let td = tempfile::tempdir().unwrap();
        let store = Store::open(td.path()).await.unwrap();
        let other = Store::open(td.path()).await.unwrap();
        let mut listener = Listener::new(&store, "test")
            .await
            .backstop(Duration::from_millis(1));
        let before = missed_announcements();
        other.pause("x/y", "test").await.unwrap();
        assert_eq!(listener.next(&store).await, ["paused x/y"]);
        listener.next(&store).await;
        assert_eq!(
            missed_announcements(),
            before,
            "an announced commit was counted"
        );

        // Round the announcement: straight to the table, as a `sqlite3`
        // shell would.
        other.write_unannounced("DELETE FROM pauses").await;
        assert!(listener.next(&store).await.is_empty());
        // Other tests share the count; none of them misses one.
        assert!(
            missed_announcements() > before,
            "the silent commit was not counted"
        );
    }
}
