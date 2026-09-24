//! What wakes a process waiting on the loop's store: a commit to
//! `supervisor.sqlite`, from any process, seen as the write to the file
//! that a commit is — to the database itself in the rollback-journal mode
//! the store is in today, to its `-wal` in WAL mode; both are watched.
//! Nobody has to announce anything, so a write from a `sqlite3` shell or
//! an older build wakes the loop too.
//!
//! FSEvents, the `notify` crate's macOS backend that `watch.rs` uses,
//! does not report a write to a file its writer holds open. kqueue on the
//! file itself does, within microseconds of the commit; on Linux, inotify
//! on the file's directory does. No kernel API serves both, so each has a
//! backend here behind one `FileWatch`. A file deleted and made again is
//! watched again when its directory says it is back.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::store::Store;

/// How long a listener waits for a wake before looking at the store
/// anyway. Only a wake that never came makes the look find anything.
pub const BACKSTOP: Duration = Duration::from_secs(5);

static MISSED: AtomicU64 = AtomicU64::new(0);

/// How many times, in this process, a backstop found a commit no wake
/// announced. Any at all is a bug; tests assert none.
pub fn missed_wakes() -> u64 {
    MISSED.load(Ordering::Relaxed)
}

/// Wakes at a write to the store (or to any `also` file), or after
/// [`BACKSTOP`] with none. A backstop that finds the store moved with no
/// wake is a bug in the watching, and says so at ERROR.
pub struct Listener {
    watch: Option<FileWatch>,
    /// Who is listening, for the ERROR line.
    who: &'static str,
    backstop: Duration,
    heard: bool,
    checked: Option<i64>,
}

impl Listener {
    pub async fn new(store: &Store, who: &'static str, also: &[PathBuf]) -> Listener {
        let db = store.path();
        let mut files = vec![db.to_path_buf(), wal_of(db)];
        files.extend(also.iter().cloned());
        let watch = match FileWatch::new(files) {
            Ok(watch) => Some(watch),
            Err(e) => {
                tracing::error!(
                    "{who}: cannot watch the store, so only its backstop wakes it: {e:#}"
                );
                None
            }
        };
        Listener {
            watch,
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

    /// Resolves at a wake, or when the backstop has looked. Safe to drop
    /// mid-wait: a wake is used up only by the call that returns it.
    pub async fn next(&mut self, store: &Store) {
        // Everything committed by the wake just heard was announced; only
        // what lands after this is the backstop's to judge.
        if self.heard {
            self.checked = store.data_version().await.ok();
            self.heard = false;
        }
        let backstop = tokio::time::sleep(self.backstop);
        tokio::select! {
            biased;
            _ = changed(&mut self.watch) => {
                self.heard = true;
            }
            _ = backstop => {
                let now = store.data_version().await.ok();
                let moved = self.checked.is_some() && now.is_some() && now != self.checked;
                let woke = self.watch.as_mut().is_some_and(FileWatch::drain);
                if moved && !woke {
                    MISSED.fetch_add(1, Ordering::Relaxed);
                    tracing::error!(
                        "{}: the loop's store changed and no watch saw it; found by the \
                         {:?} backstop, so whatever was written waited that long",
                        self.who,
                        self.backstop
                    );
                }
                self.checked = now;
            }
        }
    }
}

async fn changed(watch: &mut Option<FileWatch>) {
    match watch {
        Some(watch) => watch.changed().await,
        None => std::future::pending().await,
    }
}

pub fn wal_of(db: &Path) -> PathBuf {
    let mut wal = db.as_os_str().to_owned();
    wal.push("-wal");
    PathBuf::from(wal)
}

pub use imp::FileWatch;

#[cfg(target_os = "macos")]
mod imp {
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::path::PathBuf;

    use anyhow::{Context, Result};
    use tokio::io::unix::AsyncFd;
    use tokio::io::Interest;

    /// The `udata` a directory's registration carries.
    const DIR: usize = usize::MAX;
    const FILE_EVENTS: u32 = libc::NOTE_WRITE
        | libc::NOTE_EXTEND
        | libc::NOTE_DELETE
        | libc::NOTE_RENAME
        | libc::NOTE_REVOKE;

    /// A set of files, each watched for writes, re-watched when replaced.
    pub struct FileWatch {
        kq: AsyncFd<OwnedFd>,
        files: Vec<(PathBuf, Option<OwnedFd>)>,
        _dirs: Vec<OwnedFd>,
    }

    fn open_evtonly(path: &std::path::Path) -> Option<OwnedFd> {
        let c = std::ffi::CString::new(path.as_os_str().as_encoded_bytes()).ok()?;
        // SAFETY: a valid NUL-terminated path.
        let fd = unsafe { libc::open(c.as_ptr(), libc::O_EVTONLY | libc::O_CLOEXEC) };
        // SAFETY: a descriptor just opened, owned by nobody else.
        (fd >= 0).then(|| unsafe { OwnedFd::from_raw_fd(fd) })
    }

    impl FileWatch {
        pub fn new(files: Vec<PathBuf>) -> Result<FileWatch> {
            // SAFETY: no arguments.
            let kq = unsafe { libc::kqueue() };
            anyhow::ensure!(kq >= 0, "kqueue: {}", std::io::Error::last_os_error());
            // SAFETY: a descriptor just opened, owned by nobody else.
            let kq = unsafe { OwnedFd::from_raw_fd(kq) };
            // Read interest only: a kqueue descriptor refuses a write filter.
            let kq = AsyncFd::with_interest(kq, Interest::READABLE).context("register kqueue")?;
            let mut dirs = Vec::new();
            let mut seen = Vec::new();
            for file in &files {
                let Some(dir) = file.parent() else { continue };
                if seen.contains(&dir) {
                    continue;
                }
                seen.push(dir);
                let fd = open_evtonly(dir)
                    .with_context(|| format!("open {} to watch", dir.display()))?;
                register(kq.get_ref(), fd.as_raw_fd(), libc::NOTE_WRITE, DIR)?;
                dirs.push(fd);
            }
            let mut watch = FileWatch {
                kq,
                files: files.into_iter().map(|f| (f, None)).collect(),
                _dirs: dirs,
            };
            for i in 0..watch.files.len() {
                watch.arm(i)?;
            }
            Ok(watch)
        }

        /// Watch file `i` if it exists and is not watched; whether it is
        /// newly watched, which is to say it has just appeared.
        fn arm(&mut self, i: usize) -> Result<bool> {
            let (path, fd) = &mut self.files[i];
            if fd.is_some() {
                return Ok(false);
            }
            let Some(opened) = open_evtonly(path) else {
                return Ok(false);
            };
            register(self.kq.get_ref(), opened.as_raw_fd(), FILE_EVENTS, i)?;
            *fd = Some(opened);
            Ok(true)
        }

        /// Take every pending event; whether any says a file changed.
        pub fn drain(&mut self) -> bool {
            let mut woke = false;
            loop {
                // SAFETY: a zeroed kevent is a valid value to be overwritten.
                let mut evs: [libc::kevent; 16] = unsafe { std::mem::zeroed() };
                let zero = libc::timespec {
                    tv_sec: 0,
                    tv_nsec: 0,
                };
                // SAFETY: a live kqueue, an output buffer of its stated
                // length, and a zero timeout.
                let n = unsafe {
                    libc::kevent(
                        self.kq.get_ref().as_raw_fd(),
                        std::ptr::null(),
                        0,
                        evs.as_mut_ptr(),
                        evs.len() as i32,
                        &zero,
                    )
                };
                if n <= 0 {
                    return woke;
                }
                for ev in &evs[..n as usize] {
                    let tag = ev.udata as usize;
                    if tag == DIR {
                        for i in 0..self.files.len() {
                            woke |= self.arm(i).unwrap_or(false);
                        }
                        continue;
                    }
                    if ev.fflags & (libc::NOTE_WRITE | libc::NOTE_EXTEND) != 0 {
                        woke = true;
                    }
                    if ev.fflags & (libc::NOTE_DELETE | libc::NOTE_RENAME | libc::NOTE_REVOKE) != 0
                    {
                        // Closing the descriptor drops its registration.
                        self.files[tag].1 = None;
                        woke |= self.arm(tag).unwrap_or(false);
                    }
                }
            }
        }

        pub async fn changed(&mut self) {
            loop {
                let Ok(mut ready) = self.kq.readable_mut().await else {
                    return std::future::pending().await;
                };
                ready.clear_ready();
                drop(ready);
                if self.drain() {
                    return;
                }
            }
        }
    }

    fn register(kq: &OwnedFd, fd: i32, fflags: u32, tag: usize) -> Result<()> {
        let ev = libc::kevent {
            ident: fd as usize,
            filter: libc::EVFILT_VNODE,
            flags: libc::EV_ADD | libc::EV_CLEAR,
            fflags,
            data: 0,
            udata: tag as *mut libc::c_void,
        };
        // SAFETY: a live kqueue and one change of its stated length.
        let rc = unsafe {
            libc::kevent(
                kq.as_raw_fd(),
                &ev,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        anyhow::ensure!(rc >= 0, "kevent: {}", std::io::Error::last_os_error());
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::ffi::OsString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::unix::ffi::OsStringExt;
    use std::path::PathBuf;

    use anyhow::{Context, Result};
    use tokio::io::unix::AsyncFd;

    /// A set of files, each watched for writes through its directory, so
    /// a file replaced or made again is still watched.
    pub struct FileWatch {
        fd: AsyncFd<OwnedFd>,
        /// (watch descriptor of its directory, its name there)
        files: Vec<(i32, OsString)>,
    }

    impl FileWatch {
        pub fn new(files: Vec<PathBuf>) -> Result<FileWatch> {
            // SAFETY: flags only.
            let fd = unsafe { libc::inotify_init1(libc::IN_NONBLOCK | libc::IN_CLOEXEC) };
            anyhow::ensure!(
                fd >= 0,
                "inotify_init1: {}",
                std::io::Error::last_os_error()
            );
            // SAFETY: a descriptor just opened, owned by nobody else.
            let fd =
                AsyncFd::new(unsafe { OwnedFd::from_raw_fd(fd) }).context("register inotify")?;
            let mut watched = Vec::new();
            for file in files {
                let (Some(dir), Some(name)) = (file.parent(), file.file_name()) else {
                    continue;
                };
                let c = std::ffi::CString::new(dir.as_os_str().as_encoded_bytes())?;
                // SAFETY: a live inotify descriptor and a valid path. The
                // same directory twice returns the same descriptor.
                let wd = unsafe {
                    libc::inotify_add_watch(
                        fd.get_ref().as_raw_fd(),
                        c.as_ptr(),
                        libc::IN_MODIFY
                            | libc::IN_CREATE
                            | libc::IN_MOVED_TO
                            | libc::IN_CLOSE_WRITE,
                    )
                };
                anyhow::ensure!(
                    wd >= 0,
                    "watch {}: {}",
                    dir.display(),
                    std::io::Error::last_os_error()
                );
                watched.push((wd, name.to_owned()));
            }
            Ok(FileWatch { fd, files: watched })
        }

        /// Take every pending event; whether any names a watched file.
        pub fn drain(&mut self) -> bool {
            #[repr(C, align(8))]
            struct Buf([u8; 4096]);
            let mut buf = Buf([0; 4096]);
            let mut woke = false;
            loop {
                // SAFETY: a live descriptor and a buffer of its stated length.
                let n = unsafe {
                    libc::read(
                        self.fd.get_ref().as_raw_fd(),
                        buf.0.as_mut_ptr().cast(),
                        buf.0.len(),
                    )
                };
                if n <= 0 {
                    return woke;
                }
                let head = std::mem::size_of::<libc::inotify_event>();
                let mut at = 0usize;
                while at + head <= n as usize {
                    // SAFETY: the kernel wrote a whole event header here.
                    let ev: libc::inotify_event =
                        unsafe { std::ptr::read_unaligned(buf.0[at..].as_ptr().cast()) };
                    let name_bytes = &buf.0[at + head..at + head + ev.len as usize];
                    let end = name_bytes
                        .iter()
                        .position(|&b| b == 0)
                        .unwrap_or(name_bytes.len());
                    let name = OsString::from_vec(name_bytes[..end].to_vec());
                    woke |= self.files.iter().any(|(wd, n)| *wd == ev.wd && *n == name);
                    at += head + ev.len as usize;
                }
            }
        }

        pub async fn changed(&mut self) {
            loop {
                let Ok(mut ready) = self.fd.readable_mut().await else {
                    return std::future::pending().await;
                };
                ready.clear_ready();
                drop(ready);
                if self.drain() {
                    return;
                }
            }
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
mod imp {
    use std::path::PathBuf;

    pub struct FileWatch;

    impl FileWatch {
        pub fn new(_: Vec<PathBuf>) -> anyhow::Result<FileWatch> {
            anyhow::bail!("no file watch on this platform")
        }

        pub fn drain(&mut self) -> bool {
            false
        }

        pub async fn changed(&mut self) {
            std::future::pending().await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The loop's whole cue: another connection's commit wakes a
    /// listener, with nothing rung and nothing polled.
    #[tokio::test]
    async fn another_connections_commit_wakes_the_listener() {
        let td = tempfile::tempdir().unwrap();
        let store = Store::open(td.path()).await.unwrap();
        let other = Store::open(td.path()).await.unwrap();
        let mut listener = Listener::new(&store, "test", &[]).await;
        other.pause("a/raw", "test").await.unwrap();
        listener.next(&store).await;
        assert!(listener.heard, "woken by the backstop, not the watch");
    }

    /// A watched file deleted and made again, as SQLite does to its WAL
    /// when the last connection closes, is heard through its new self.
    #[tokio::test]
    async fn a_file_deleted_and_made_again_still_wakes() {
        let td = tempfile::tempdir().unwrap();
        let file = td.path().join("store.sqlite-wal");
        std::fs::write(&file, "a").unwrap();
        let mut watch = FileWatch::new(vec![file.clone()]).unwrap();
        std::fs::remove_file(&file).unwrap();
        watch.drain();
        // Its return wakes by itself; only the write after is heard
        // through the new file's watch.
        std::fs::write(&file, "b").unwrap();
        watch.changed().await;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&file)
            .unwrap();
        std::io::Write::write_all(&mut f, b"c").unwrap();
        watch.changed().await;
    }

    /// A file written in place, and one replaced by a rename — how an
    /// editor saves `config.toml` — both wake.
    #[tokio::test]
    async fn a_file_written_or_replaced_wakes() {
        let td = tempfile::tempdir().unwrap();
        let file = td.path().join("config.toml");
        std::fs::write(&file, "a").unwrap();
        let mut watch = FileWatch::new(vec![file.clone()]).unwrap();
        std::fs::write(&file, "b").unwrap();
        watch.changed().await;
        let tmp = td.path().join("config.toml.tmp");
        std::fs::write(&tmp, "c").unwrap();
        std::fs::rename(&tmp, &file).unwrap();
        watch.changed().await;
        std::fs::write(&file, "d").unwrap();
        watch.changed().await;
    }

    /// The backstop's whole job: a commit no watch saw is found, and
    /// counted as the bug it is. Written to a file the listener does not
    /// watch, standing in for a watch that failed.
    #[tokio::test]
    async fn a_commit_no_watch_saw_is_found_by_the_backstop_and_counted() {
        let td = tempfile::tempdir().unwrap();
        let store = Store::open(td.path()).await.unwrap();
        let other = Store::open(td.path()).await.unwrap();
        let mut listener = Listener::new(&store, "test", &[])
            .await
            .backstop(Duration::from_millis(1));
        listener.watch = None;
        let before = missed_wakes();
        other.pause("a/raw", "test").await.unwrap();
        listener.next(&store).await;
        // Other tests share the count; none of them misses one.
        assert!(missed_wakes() > before, "the unseen commit was not counted");
    }
}
