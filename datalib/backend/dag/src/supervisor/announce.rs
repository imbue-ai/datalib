//! How a process waiting on the loop hears that something it cares about
//! happened: whoever did it says so, the way a step says it sealed. `Store`
//! announces after every commit it makes; the server announces a config
//! change; a process letting go of `runner-lock` announces that.
//!
//! A listener is a FIFO in `system/supervisor-listeners/`, and an
//! announcement is one line written to every FIFO there: who made it, and
//! what it was. A FIFO nobody
//! has open belongs to a process that is gone, and announcing removes it.
//! Why announcements and not a watch on the database file:
//! `dag/README.md` § "What wakes the loop".

use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::time::Instant;

use super::store::Store;

/// How long a listener hears nothing before it looks at the store itself.
pub const BACKSTOP: Duration = Duration::from_secs(30);

/// Who announced, for the lines `Store` does not make.
pub const FROM_SERVER: &str = "server";
pub const FROM_LOCK: &str = "lock";

pub const CONFIG_CHANGED: &str = "config changed";
pub const RUNNER_LOCK_RELEASED: &str = "runner-lock released";
/// What [`Listener::next`] returns when its backstop, not an announcement,
/// found the store moved.
pub const UNANNOUNCED: &str = "unannounced change";

/// Longest line that reaches a FIFO in one write, so two announcers'
/// lines never interleave: POSIX's `PIPE_BUF` floor.
const MAX_LINE: usize = 512;

static MISSED: AtomicU64 = AtomicU64::new(0);

/// How many commits, in this process, a backstop found that nobody
/// announced. Any at all is a writer that bypasses `Store`; tests assert
/// none.
pub fn missed_announcements() -> u64 {
    MISSED.load(Ordering::Relaxed)
}

pub fn listeners_dir(data_root: &Path) -> PathBuf {
    datalib_runtime::layout::supervisor_db(data_root).with_file_name("supervisor-listeners")
}

/// Tell every listener in `dir` that `from` did `what`. Never fails: a
/// listener that misses a line still has its backstop, which says so.
pub fn announce(dir: &Path, from: &str, what: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut line = format!("{from} {what}").replace('\n', " ");
    line.truncate(line.floor_char_boundary(MAX_LINE - 1));
    line.push('\n');
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "fifo") {
            tell(&path, line.as_bytes());
        }
    }
}

fn tell(fifo: &Path, line: &[u8]) {
    let opened = std::fs::OpenOptions::new()
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(fifo);
    match opened {
        // A full pipe already has plenty to wake its reader.
        Ok(mut f) => {
            let _ = f.write(line);
        }
        Err(e) if e.raw_os_error() == Some(libc::ENXIO) => {
            let _ = std::fs::remove_file(fifo);
        }
        Err(_) => {}
    }
}

/// One process's ear on the store. Make it before reading the state it
/// guards: a line announced between the read and the wait is held in the
/// FIFO until [`Listener::next`] takes it.
///
/// It never uses a connection to the store, and that is load-bearing: it
/// is waited on in `select!` and under `timeout`, and a query dropped
/// mid-flight can leave a single-connection pool unable to serve the
/// caller's next one. Its backstop reads the database file's own change
/// counter instead.
pub struct Listener {
    fifo: Option<Fifo>,
    who: &'static str,
    backstop: Duration,
    wake_at: Instant,
    /// Heard, and not yet returned: kept across a cancelled `next`.
    pending: Vec<String>,
    /// The store this listener was made from. What it announced itself
    /// wakes nobody: the loop would otherwise tick again after every save
    /// of its own record.
    own: String,
    /// The database file, whose change counter the backstop reads.
    db: PathBuf,
    /// The change counter as of the last announcement heard.
    checked: Option<u32>,
    /// The backstop saw the store move with nothing announced; if nothing
    /// is announced by its next look either, that commit was missed.
    suspect: bool,
}

impl Listener {
    /// `who` names the listener in the ERROR line a missed commit costs.
    pub fn new(store: &Store, who: &'static str) -> Listener {
        let fifo = match Fifo::make(store.listeners()) {
            Ok(fifo) => Some(fifo),
            Err(e) => {
                tracing::error!("{who}: cannot listen, so only the backstop wakes it: {e:#}");
                None
            }
        };
        Listener {
            fifo,
            who,
            backstop: BACKSTOP,
            wake_at: Instant::now() + BACKSTOP,
            pending: Vec::new(),
            own: store.me().to_string(),
            checked: change_counter(store.path()),
            db: store.path().to_path_buf(),
            suspect: false,
        }
    }

    pub fn backstop(mut self, after: Duration) -> Listener {
        self.backstop = after;
        self.wake_at = Instant::now() + after;
        self
    }

    /// What was announced since the last call, by anyone but the store it
    /// was made from, waiting if nothing was; `[UNANNOUNCED]` when the
    /// backstop found the store moved. Never empty. Safe to drop at any
    /// point: it waits only on its FIFO and a timer.
    pub async fn next(&mut self) -> Vec<String> {
        loop {
            self.drain();
            if !self.pending.is_empty() {
                return std::mem::take(&mut self.pending);
            }
            tokio::select! {
                biased;
                () = readable(&self.fifo) => {}
                () = tokio::time::sleep_until(self.wake_at) => {
                    // What arrived as the timer fired is heard, not missed.
                    if self.drain() {
                        continue;
                    }
                    if let Some(found) = self.look() {
                        return found;
                    }
                }
            }
        }
    }

    /// Whether anything was announced, its own lines included: each is a
    /// commit accounted for, so the backstop starts over from here.
    fn drain(&mut self) -> bool {
        let Some(fifo) = self.fifo.as_mut() else {
            return false;
        };
        let lines = fifo.drain();
        if lines.is_empty() {
            return false;
        }
        for line in lines {
            let (from, what) = line.split_once(' ').unwrap_or(("", &line));
            if from != self.own {
                self.pending.push(what.to_string());
            }
        }
        self.checked = change_counter(&self.db);
        self.suspect = false;
        self.wake_at = Instant::now() + self.backstop;
        true
    }

    fn look(&mut self) -> Option<Vec<String>> {
        self.wake_at = Instant::now() + self.backstop;
        let now = change_counter(&self.db)?;
        if self.checked.is_none_or(|c| c == now) {
            self.checked = Some(now);
            self.suspect = false;
            return None;
        }
        if std::mem::replace(&mut self.suspect, true) {
            MISSED.fetch_add(1, Ordering::Relaxed);
            tracing::error!(
                "{}: the loop's store changed and nobody announced it; the backstop found it \
                 after {:?}, so whatever was written waited that long",
                self.who,
                self.backstop
            );
            self.checked = Some(now);
            self.suspect = false;
        }
        Some(vec![UNANNOUNCED.to_string()])
    }
}

/// SQLite's file change counter (header bytes 24–27), which every commit
/// moves in rollback-journal mode, whichever connection made it.
fn change_counter(db: &Path) -> Option<u32> {
    let mut header = [0u8; 28];
    File::open(db).ok()?.read_exact(&mut header).ok()?;
    Some(u32::from_be_bytes([
        header[24], header[25], header[26], header[27],
    ]))
}

/// Resolves when the FIFO may have something to read, forgetting that it
/// did: the drain that follows reads whatever is there.
async fn readable(fifo: &Option<Fifo>) {
    match fifo {
        Some(fifo) => match fifo.rx.readable().await {
            Ok(mut ready) => ready.clear_ready(),
            Err(_) => std::future::pending().await,
        },
        None => std::future::pending().await,
    }
}

/// A listener's FIFO. Dropping it takes the FIFO away.
struct Fifo {
    path: PathBuf,
    /// Read with plain non-blocking reads: tokio's own reads report
    /// nothing until its reactor has seen the pipe become readable.
    rx: AsyncFd<File>,
    /// Our own write end: with no writer open, a read sees end-of-file at
    /// once instead of waiting.
    _hold: File,
    partial: Vec<u8>,
}

/// Tests in one binary share a pid.
static NEXT: AtomicU64 = AtomicU64::new(0);

impl Fifo {
    fn make(dir: &Path) -> Result<Fifo> {
        std::fs::create_dir_all(dir).with_context(|| format!("create {}", dir.display()))?;
        let name = format!(
            "{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        // Made under a name `announce` skips, and renamed once it has a
        // reader: an announcer that opened it first would find no reader
        // and remove it.
        let making = dir.join(format!("{name}.making"));
        let path = dir.join(format!("{name}.fifo"));
        let _ = std::fs::remove_file(&making);
        let c = std::ffi::CString::new(making.as_os_str().as_encoded_bytes())
            .context("a listener's path holds a NUL")?;
        // SAFETY: a NUL-terminated path and a mode.
        if unsafe { libc::mkfifo(c.as_ptr(), 0o600) } != 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("mkfifo {}", making.display()));
        }
        let read = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&making)
            .with_context(|| format!("open {} to listen", making.display()))?;
        let rx = AsyncFd::with_interest(read, Interest::READABLE)
            .with_context(|| format!("watch {}", making.display()))?;
        let hold = std::fs::OpenOptions::new()
            .write(true)
            .custom_flags(libc::O_NONBLOCK)
            .open(&making)
            .with_context(|| format!("open {} to hold", making.display()))?;
        std::fs::rename(&making, &path)
            .with_context(|| format!("rename {} into place", making.display()))?;
        Ok(Fifo {
            path,
            rx,
            _hold: hold,
            partial: Vec::new(),
        })
    }

    /// Every whole line waiting, without waiting for more.
    fn drain(&mut self) -> Vec<String> {
        let mut buf = [0u8; 4096];
        while let Ok(n @ 1..) = self.rx.get_ref().read(&mut buf) {
            self.partial.extend_from_slice(&buf[..n]);
        }
        let mut lines = Vec::new();
        while let Some(at) = self.partial.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.partial.drain(..=at).collect();
            lines.push(String::from_utf8_lossy(&line[..at]).into_owned());
        }
        lines
    }
}

impl Drop for Fifo {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The loop's whole cue: another connection's commit reaches every
    /// listener, saying what it was, including one made before the
    /// listener started waiting.
    #[tokio::test]
    async fn a_commit_reaches_every_listener_even_one_not_yet_waiting() {
        let td = tempfile::tempdir().unwrap();
        let store = Store::open(td.path()).await.unwrap();
        let other = Store::open(td.path()).await.unwrap();
        let mut a = Listener::new(&store, "a");
        let mut b = Listener::new(&store, "b");
        other.pause("x/y", "test").await.unwrap();
        assert_eq!(a.next().await, ["paused x/y"]);
        assert_eq!(b.next().await, ["paused x/y"]);
    }

    /// A listener is waited on under `select!` and `timeout`, and dropped
    /// wherever it is. When it read the store's `data_version` after
    /// hearing a line, a drop in that query could leave the store's one
    /// connection unable to answer the caller's next query: the loop hung
    /// on its next save (harness walk seed 237), and so could every
    /// handler sharing the server's mailbox connection.
    #[tokio::test]
    async fn a_listener_dropped_mid_wait_leaves_the_store_usable() {
        let td = tempfile::tempdir().unwrap();
        let store = Store::open(td.path()).await.unwrap();
        let other = Store::open(td.path()).await.unwrap();
        let mut listener = Listener::new(&store, "test").backstop(Duration::ZERO);
        for i in 0..50 {
            other.pause(&format!("s{i}"), "test").await.unwrap();
            let _ = tokio::time::timeout(Duration::ZERO, listener.next()).await;
            tokio::time::timeout(Duration::from_secs(5), store.paused())
                .await
                .expect("the store answers after a dropped wait")
                .unwrap();
        }
    }

    /// A dead process's FIFO would otherwise cost every announcement an
    /// open that goes nowhere, for good.
    #[tokio::test]
    async fn announcing_removes_a_fifo_nobody_listens_on() {
        let td = tempfile::tempdir().unwrap();
        let dir = listeners_dir(td.path());
        std::fs::create_dir_all(&dir).unwrap();
        let orphan = dir.join("1-0.fifo");
        let c = std::ffi::CString::new(orphan.as_os_str().as_encoded_bytes()).unwrap();
        // SAFETY: a NUL-terminated path and a mode.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
        announce(&dir, FROM_SERVER, "anything");
        assert!(!orphan.exists());
    }

    /// The loop saves its record after every tick; hearing its own save
    /// would cost a tick per tick.
    #[tokio::test]
    async fn a_listener_does_not_hear_its_own_store() {
        let td = tempfile::tempdir().unwrap();
        let store = Store::open(td.path()).await.unwrap();
        let other = Store::open(td.path()).await.unwrap();
        let mut listener = Listener::new(&store, "test");
        store.pause("mine", "test").await.unwrap();
        other.pause("theirs", "test").await.unwrap();
        assert_eq!(listener.next().await, ["paused theirs"]);
    }

    #[tokio::test]
    async fn a_dropped_listener_takes_its_fifo_with_it() {
        let td = tempfile::tempdir().unwrap();
        let store = Store::open(td.path()).await.unwrap();
        let listener = Listener::new(&store, "test");
        let count = || std::fs::read_dir(listeners_dir(td.path())).unwrap().count();
        assert_eq!(count(), 1);
        drop(listener);
        assert_eq!(count(), 0);
    }

    /// A line longer than a pipe writes at once would interleave with
    /// another announcer's; it is cut instead.
    #[tokio::test]
    async fn a_long_announcement_arrives_as_one_line() {
        let td = tempfile::tempdir().unwrap();
        let store = Store::open(td.path()).await.unwrap();
        let mut listener = Listener::new(&store, "test");
        announce(store.listeners(), FROM_SERVER, &"é".repeat(400));
        let heard = listener.next().await;
        assert_eq!(heard.len(), 1);
        assert!(heard[0].len() < MAX_LINE);
    }

    /// The backstop's whole job: a commit nobody announced wakes the
    /// listener and, once nothing is announced for it by the next look,
    /// is counted as the bug it is. One that was announced is not.
    #[tokio::test]
    async fn a_commit_nobody_announced_wakes_the_backstop_and_is_counted() {
        let td = tempfile::tempdir().unwrap();
        let store = Store::open(td.path()).await.unwrap();
        let other = Store::open(td.path()).await.unwrap();
        let mut listener = Listener::new(&store, "test").backstop(Duration::from_millis(1));
        other.pause("x/y", "test").await.unwrap();
        assert_eq!(listener.next().await, ["paused x/y"]);

        // Past `Store`, as a `sqlite3` shell would write.
        let before = missed_announcements();
        sqlx::query("DELETE FROM pauses")
            .execute(other.pool())
            .await
            .unwrap();
        assert_eq!(listener.next().await, [UNANNOUNCED]);
        assert_eq!(listener.next().await, [UNANNOUNCED]);
        // Other tests share the count, and none of them misses one.
        assert!(missed_announcements() > before);
    }

    /// A listener that cannot make its FIFO still wakes, on the backstop
    /// alone: it hears no announcement, so every commit reads as missed.
    #[tokio::test]
    async fn a_listener_that_cannot_listen_wakes_on_the_backstop() {
        let td = tempfile::tempdir().unwrap();
        let store = Store::open(td.path()).await.unwrap();
        let other = Store::open(td.path()).await.unwrap();
        let _ = std::fs::remove_dir_all(store.listeners());
        std::fs::write(store.listeners(), "not a directory").unwrap();
        let mut listener = Listener::new(&store, "test").backstop(Duration::from_millis(1));
        other.pause("x/y", "test").await.unwrap();
        assert_eq!(listener.next().await, [UNANNOUNCED]);
    }
}
