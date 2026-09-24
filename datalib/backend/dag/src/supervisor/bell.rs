//! A doorbell beside the loop's store: whoever writes the store rings it,
//! and every process listening hears it at once. A SQLite commit reaches
//! no other connection by itself, and a file watch does not see it either
//! — macOS does not announce an append to a WAL its writer holds open — so
//! without this a reader can only poll.
//!
//! A listener is a FIFO in `system/supervisor-bells/`; a ring writes one
//! byte to each. A FIFO's path, unlike a Unix socket's, has no length
//! limit, and a data root can be deep. A FIFO nobody has open is one whose
//! process is gone, and the ring removes it.

use std::path::{Path, PathBuf};

pub fn bells_dir(data_root: &Path) -> PathBuf {
    datalib_runtime::layout::supervisor_db(data_root).with_file_name("supervisor-bells")
}

/// Write a byte to every listener's FIFO. Never fails: a listener that
/// misses a ring still has the backstop poll, which says so.
pub fn ring(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "fifo") {
            continue;
        }
        imp::ring_one(&path);
    }
}

#[cfg(unix)]
mod imp {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    use std::path::Path;

    pub fn ring_one(path: &Path) {
        let opened = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(path);
        match opened {
            // A full pipe is a bell already ringing.
            Ok(mut fifo) => {
                let _ = fifo.write(&[1]);
            }
            // No reader: its process is gone.
            Err(e) if e.raw_os_error() == Some(libc::ENXIO) => {
                let _ = std::fs::remove_file(path);
            }
            Err(_) => {}
        }
    }
}

#[cfg(not(unix))]
mod imp {
    pub fn ring_one(_: &std::path::Path) {}
}

#[cfg(unix)]
pub use listen::Bell;

/// How long a listener waits for a ring before looking at the store
/// anyway. Only a ring that never came makes the look find anything.
pub const BACKSTOP: std::time::Duration = std::time::Duration::from_secs(5);

/// How often a listener with no bell looks instead.
#[cfg(not(unix))]
const POLL: std::time::Duration = std::time::Duration::from_millis(250);

static MISSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// How many times, in this process, a backstop found a write nobody rang
/// for. Any at all is a bug; tests assert none.
pub fn missed_rings() -> u64 {
    MISSED.load(std::sync::atomic::Ordering::Relaxed)
}

/// A bell with its backstop: wakes at a ring, or after [`BACKSTOP`]
/// with nothing rung. A backstop that finds the store moved with no ring
/// is a bug in the ringing, and says so at ERROR.
pub struct Listener {
    #[cfg(unix)]
    bell: Option<Bell>,
    /// Who is listening, for the ERROR line.
    who: &'static str,
    backstop: std::time::Duration,
    heard: bool,
    checked: Option<i64>,
}

impl Listener {
    pub async fn new(store: &super::store::Store, who: &'static str) -> Listener {
        #[cfg(unix)]
        let bell = match Bell::listen(store.bells()) {
            Ok(bell) => Some(bell),
            Err(e) => {
                tracing::error!("{who}: cannot listen for writes, so it will only poll: {e:#}");
                None
            }
        };
        Listener {
            #[cfg(unix)]
            bell,
            who,
            backstop: BACKSTOP,
            heard: false,
            checked: store.data_version().await.ok(),
        }
    }

    pub fn backstop(mut self, after: std::time::Duration) -> Listener {
        self.backstop = after;
        self
    }

    /// Resolves at a ring, or when the backstop has looked. Safe to drop
    /// mid-wait: a ring is used up only by the call that returns it.
    pub async fn next(&mut self, store: &super::store::Store) {
        // Everything committed by the ring just heard was announced; only
        // what lands after this is the backstop's to judge.
        if self.heard {
            self.checked = store.data_version().await.ok();
            self.heard = false;
        }
        let backstop = tokio::time::sleep(self.backstop);
        tokio::select! {
            biased;
            _ = self.rung() => {
                self.heard = true;
            }
            _ = backstop => {
                let now = store.data_version().await.ok();
                let moved = self.checked.is_some() && now.is_some() && now != self.checked;
                if moved && !self.drain() {
                    MISSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    tracing::error!(
                        "{}: the loop's store changed and no ring said so; found by the \
                         {:?} backstop, so whatever was written waited that long",
                        self.who,
                        self.backstop
                    );
                }
                self.checked = now;
            }
        }
    }

    #[cfg(unix)]
    async fn rung(&mut self) {
        match &mut self.bell {
            Some(bell) => bell.rung().await,
            None => std::future::pending().await,
        }
    }

    #[cfg(not(unix))]
    async fn rung(&mut self) {
        tokio::time::sleep(POLL).await
    }

    #[cfg(unix)]
    fn drain(&mut self) -> bool {
        self.bell.as_mut().is_some_and(Bell::drain)
    }

    #[cfg(not(unix))]
    fn drain(&mut self) -> bool {
        true
    }
}

#[cfg(unix)]
mod listen {
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};

    use anyhow::{Context, Result};
    use tokio::net::unix::pipe;

    /// One listener. Dropping it takes its FIFO away.
    pub struct Bell {
        path: PathBuf,
        rx: pipe::Receiver,
        /// Held so the FIFO always has a writer: with none, a read sees
        /// end-of-file at once instead of waiting for a ring.
        _writer: pipe::Sender,
    }

    /// Tests in one binary share a pid.
    static NEXT: AtomicU64 = AtomicU64::new(0);

    impl Bell {
        /// Must be called inside a tokio runtime.
        pub fn listen(dir: &Path) -> Result<Bell> {
            std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
            let name = format!(
                "{}-{}.fifo",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            );
            let path = dir.join(name);
            let _ = std::fs::remove_file(&path);
            let c_path = std::ffi::CString::new(path.as_os_str().as_encoded_bytes())
                .context("a bell's path holds a NUL")?;
            // SAFETY: a valid NUL-terminated path, and a mode.
            if unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) } != 0 {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("mkfifo {}", path.display()));
            }
            let rx = pipe::OpenOptions::new()
                .open_receiver(&path)
                .with_context(|| format!("open {} to listen", path.display()))?;
            let writer = pipe::OpenOptions::new()
                .open_sender(&path)
                .with_context(|| format!("open {} to hold", path.display()))?;
            Ok(Bell {
                path,
                rx,
                _writer: writer,
            })
        }

        /// Resolves at the next ring, or at once if one came since the
        /// last call; every ring before it is used up.
        pub async fn rung(&mut self) {
            loop {
                if self.rx.readable().await.is_err() {
                    return std::future::pending().await;
                }
                if self.drain() {
                    return;
                }
            }
        }

        /// Whether a ring came since the last call, without waiting.
        pub fn drain(&mut self) -> bool {
            let mut buf = [0u8; 256];
            let mut any = false;
            loop {
                match self.rx.try_read(&mut buf) {
                    Ok(0) => return any,
                    Ok(_) => any = true,
                    Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => return any,
                    Err(_) => return any,
                }
            }
        }
    }

    impl Drop for Bell {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// The loop's whole cue: a ring from anyone reaches every listener,
    /// and one that comes before the listener waits is not lost.
    #[tokio::test]
    async fn a_ring_reaches_every_listener_and_waits_for_one_not_yet_listening() {
        let td = tempfile::tempdir().unwrap();
        let dir = bells_dir(td.path());
        let mut a = Bell::listen(&dir).unwrap();
        let mut b = Bell::listen(&dir).unwrap();
        assert!(!a.drain() && !b.drain());
        ring(&dir);
        a.rung().await;
        b.rung().await;
        assert!(!a.drain(), "a ring is used up once heard");
    }

    /// The backstop's whole job: a write that came with no ring is found,
    /// and counted as the bug it is; a write that rang is not.
    #[tokio::test]
    async fn a_write_nobody_rang_for_is_found_by_the_backstop_and_counted() {
        let td = tempfile::tempdir().unwrap();
        let store = super::super::store::Store::open(td.path()).await.unwrap();
        let other = super::super::store::Store::open(td.path()).await.unwrap();
        let mut listener = Listener::new(&store, "test")
            .await
            .backstop(std::time::Duration::from_millis(1));

        let before = missed_rings();
        other.pause("a/raw", "test").await.unwrap();
        listener.next(&store).await;
        listener.next(&store).await;
        assert_eq!(missed_rings(), before, "a write that rang was counted");

        // Round the ring: straight to the table, as a build that forgot
        // to ring would.
        sqlx::query("DELETE FROM pauses")
            .execute(other.pool())
            .await
            .unwrap();
        listener.next(&store).await;
        assert_eq!(
            missed_rings(),
            before + 1,
            "the silent write was not counted"
        );
    }

    /// A listener that died without taking its FIFO away would otherwise
    /// cost every ring an open that goes nowhere, forever.
    #[tokio::test]
    async fn a_ring_removes_a_fifo_nobody_listens_on() {
        let td = tempfile::tempdir().unwrap();
        let dir = bells_dir(td.path());
        let mut live = Bell::listen(&dir).unwrap();
        let orphan = dir.join("0-0.fifo");
        let c = std::ffi::CString::new(orphan.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a valid path and a mode.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        ring(&dir);
        assert!(!orphan.exists());
        live.rung().await;
    }
}
