//! What macOS's FSEvents history does and does not vouch for, one test per
//! fact, measured so a fast path for `fsscan` can be designed around them.
//! Nothing in datalib uses FSEvents yet.
//!
//! The question each test asks is the one a sync would: "what changed under
//! this root since the event id I stored last run?" A new stream replays the
//! on-disk log from that id and says when it has caught up.
//!
//! Four facts have no test, because each takes too long, needs a reboot, or
//! did not hold every time:
//! - Replay cost grows with activity on the whole machine, not under the
//!   root: about 1s per 1M ids and 11s for 50M, on a machine producing about
//!   1.85M ids an hour, with nothing changed under the root.
//! - From a recent id, a replay over a 1.2M-file tree took 8ms.
//! - Whether the log survives a reboot is not known. A replay from the start
//!   showed no event older than the current boot, which proves nothing either
//!   way, so a stored id has to be checked against
//!   `FSEventsCopyUUIDForDevice` at the least.
//! - A burst of changes can lose events and say nothing. Touching all 1.2M
//!   files of a tree with folders of up to 1,500 files left 4-7% of them with
//!   no event and no warning flag, the same count missing three minutes later,
//!   in three runs. The same burst on 1.2M files in folders of 100 lost none,
//!   once. What decides it was not looked into.

#![cfg(target_os = "macos")]
#![allow(non_snake_case, non_upper_case_globals)]

use std::collections::HashSet;
use std::ffi::{c_void, CStr, CString};
use std::io::Write as _;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

#[repr(C)]
struct StreamContext {
    version: isize,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}
type Callback =
    extern "C" fn(*const c_void, *mut c_void, usize, *mut c_void, *const u32, *const u64);

#[link(name = "CoreFoundation", kind = "framework")]
extern "C" {}

#[link(name = "CoreServices", kind = "framework")]
extern "C" {
    static kCFRunLoopDefaultMode: *const c_void;
    static kCFTypeArrayCallBacks: c_void;
    fn CFStringCreateWithCString(
        a: *const c_void,
        s: *const libc::c_char,
        enc: u32,
    ) -> *const c_void;
    fn CFArrayCreate(
        a: *const c_void,
        v: *const *const c_void,
        n: isize,
        cb: *const c_void,
    ) -> *const c_void;
    fn CFRunLoopGetCurrent() -> *mut c_void;
    fn CFRunLoopRunInMode(mode: *const c_void, secs: f64, ret: u8) -> i32;
    fn CFRelease(p: *const c_void);
    fn FSEventStreamCreate(
        a: *const c_void,
        cb: Callback,
        ctx: *const StreamContext,
        paths: *const c_void,
        since: u64,
        latency: f64,
        flags: u32,
    ) -> *mut c_void;
    fn FSEventStreamScheduleWithRunLoop(s: *mut c_void, rl: *mut c_void, mode: *const c_void);
    fn FSEventStreamStart(s: *mut c_void) -> u8;
    fn FSEventStreamStop(s: *mut c_void);
    fn FSEventStreamInvalidate(s: *mut c_void);
    fn FSEventStreamRelease(s: *mut c_void);
    fn FSEventsGetCurrentEventId() -> u64;
    fn FSEventsGetLastEventIdForDeviceBeforeTime(dev: i32, t: f64) -> u64;
}

const CF_UTF8: u32 = 0x0800_0100;
const CREATE_FILE_EVENTS: u32 = 0x10;
const HISTORY_DONE: u32 = 0x10;

#[derive(Default)]
struct Replay {
    paths: HashSet<PathBuf>,
    done: bool,
}

extern "C" fn on_events(
    _s: *const c_void,
    info: *mut c_void,
    n: usize,
    paths: *mut c_void,
    flags: *const u32,
    _ids: *const u64,
) {
    let replay = unsafe { &mut *(info as *mut Replay) };
    let paths = paths as *const *const libc::c_char;
    for i in 0..n {
        let flag = unsafe { *flags.add(i) };
        if flag & HISTORY_DONE != 0 {
            replay.done = true;
            continue;
        }
        let path = unsafe { CStr::from_ptr(*paths.add(i)) }.to_string_lossy();
        replay
            .paths
            .insert(PathBuf::from(path.trim_end_matches('/')));
    }
}

fn current_id() -> u64 {
    unsafe { FSEventsGetCurrentEventId() }
}

/// Everything the log holds under `root` since `since`, file by file.
fn replay(root: &Path, since: u64) -> Replay {
    let state = Box::into_raw(Box::<Replay>::default());
    let root = CString::new(root.to_str().unwrap()).unwrap();
    unsafe {
        let path = CFStringCreateWithCString(std::ptr::null(), root.as_ptr(), CF_UTF8);
        let paths = CFArrayCreate(
            std::ptr::null(),
            &path,
            1,
            &kCFTypeArrayCallBacks as *const c_void,
        );
        let context = StreamContext {
            version: 0,
            info: state as *mut c_void,
            retain: std::ptr::null(),
            release: std::ptr::null(),
            copy_description: std::ptr::null(),
        };
        let stream = FSEventStreamCreate(
            std::ptr::null(),
            on_events,
            &context,
            paths,
            since,
            0.0,
            CREATE_FILE_EVENTS,
        );
        FSEventStreamScheduleWithRunLoop(stream, CFRunLoopGetCurrent(), kCFRunLoopDefaultMode);
        assert_ne!(FSEventStreamStart(stream), 0, "the stream would not start");
        let deadline = Instant::now() + Duration::from_secs(300);
        loop {
            if (*state).done {
                break;
            }
            assert!(Instant::now() < deadline, "the replay never caught up");
            CFRunLoopRunInMode(kCFRunLoopDefaultMode, 0.05, 1);
        }
        FSEventStreamStop(stream);
        FSEventStreamInvalidate(stream);
        FSEventStreamRelease(stream);
        CFRelease(paths);
        CFRelease(path);
        *Box::from_raw(state)
    }
}

/// Replay until `path` shows, and return the replay that showed it.
fn wait_for(root: &Path, since: u64, path: &Path) -> Replay {
    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let seen = replay(root, since);
        if seen.paths.contains(path) {
            return seen;
        }
        assert!(Instant::now() < deadline, "{} never showed", path.display());
        std::thread::sleep(Duration::from_millis(50));
    }
}

struct Tree {
    _dir: tempfile::TempDir,
    root: PathBuf,
    outside: PathBuf,
}

fn tree() -> Tree {
    let dir = tempfile::tempdir().unwrap();
    let base = dir.path().canonicalize().unwrap();
    let (root, outside) = (base.join("root"), base.join("outside"));
    std::fs::create_dir(&root).unwrap();
    std::fs::create_dir(&outside).unwrap();
    Tree {
        _dir: dir,
        root,
        outside,
    }
}

fn append(path: &Path) {
    std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .unwrap()
        .write_all(b"x")
        .unwrap();
}

/// A closed write under `root`, seen by a replay. Whatever happened before
/// it is in the log by the time the replay shows it, which is how a test
/// asks that something was *not* reported without sleeping.
fn sentinel(root: &Path, since: u64) -> Replay {
    let path = root.join("sentinel");
    std::fs::write(&path, b"s").unwrap();
    wait_for(root, since, &path)
}

/// Why a fast path cannot trust a folder's own stat: a write in place moves
/// nothing on the folder. Only a rename over the file does.
#[test]
fn a_write_in_place_leaves_the_folders_mtime_alone() {
    let t = tree();
    let file = t.root.join("f");
    std::fs::write(&file, b"a").unwrap();
    let stamp = || {
        let m = std::fs::metadata(&t.root).unwrap();
        (m.mtime(), m.mtime_nsec())
    };
    let before = stamp();
    append(&file);
    assert_eq!(stamp(), before, "an append");
    std::fs::write(&file, b"rewritten").unwrap();
    assert_eq!(stamp(), before, "a truncate and rewrite");
    std::fs::write(t.root.join(".tmp"), b"saved").unwrap();
    std::fs::rename(t.root.join(".tmp"), &file).unwrap();
    assert_ne!(stamp(), before, "an atomic save");
}

/// The id to store next is the one read *before* the replay, and that is
/// safe: the id read right after a write is below the write's event, so a
/// write straddling two runs is replayed by the second. Five hundred writes
/// racing a chain of replays lose none.
#[test]
fn a_chain_of_stored_ids_loses_no_closed_write() {
    let t = tree();
    let files: Vec<PathBuf> = (0..500).map(|i| t.root.join(format!("f{i}"))).collect();
    for f in &files {
        std::fs::write(f, b"0").unwrap();
    }
    let _ = sentinel(&t.root, current_id());
    let mut stored = current_id();
    let to_write = files.clone();
    let writer = std::thread::spawn(move || {
        for f in &to_write {
            append(f);
            std::thread::sleep(Duration::from_millis(2));
        }
    });
    let mut seen = HashSet::new();
    while !writer.is_finished() {
        let id = current_id();
        seen.extend(replay(&t.root, stored).paths);
        stored = id;
    }
    writer.join().unwrap();
    seen.extend(sentinel(&t.root, stored).paths);
    let lost: Vec<&PathBuf> = files.iter().filter(|f| !seen.contains(*f)).collect();
    assert!(
        lost.is_empty(),
        "{} of 500 lost: {:?}",
        lost.len(),
        &lost[..lost.len().min(3)]
    );
}

/// A file a process keeps open is not reported until it closes, however
/// long and however often it is written and fsynced, though its stat moves
/// at once. A database or a log held by a running app looks unchanged.
/// (Measured to five minutes; this test asks for less.)
#[test]
fn a_file_held_open_is_not_reported_until_it_closes() {
    let t = tree();
    let file = t.root.join("db");
    std::fs::write(&file, b"0").unwrap();
    let _ = sentinel(&t.root, current_id());
    let since = current_id();
    let before = std::fs::metadata(&file).unwrap().size();

    let mut held = std::fs::OpenOptions::new()
        .append(true)
        .open(&file)
        .unwrap();
    held.write_all(b"1").unwrap();
    held.sync_all().unwrap();
    assert_ne!(
        std::fs::metadata(&file).unwrap().size(),
        before,
        "a stat sees it"
    );
    assert!(
        !sentinel(&t.root, since).paths.contains(&file),
        "not reported while open"
    );

    drop(held);
    wait_for(&t.root, since, &file);
}

/// `fsscan` follows a symlink to a file, and the file it reads can be
/// anywhere: an edit there is no event under the root.
#[test]
fn a_symlinks_target_outside_the_root_is_not_reported() {
    let t = tree();
    let target = t.outside.join("target");
    std::fs::write(&target, b"0").unwrap();
    let link = t.root.join("link");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let _ = sentinel(&t.root, current_id());
    let since = current_id();

    append(&target);

    let seen = sentinel(&t.root, since);
    assert!(!seen.paths.contains(&link), "{:?}", seen.paths);
    assert!(!seen.paths.contains(&target));
}

/// A file with a second name is reported under the name in the root, even
/// when the write came through the other one.
#[test]
fn an_edit_through_a_hard_link_outside_the_root_is_reported() {
    let t = tree();
    let inside = t.root.join("inside");
    let outside = t.outside.join("outside");
    std::fs::write(&inside, b"0").unwrap();
    std::fs::hard_link(&inside, &outside).unwrap();
    let _ = sentinel(&t.root, current_id());
    let since = current_id();

    append(&outside);

    wait_for(&t.root, since, &inside);
}

/// Every kind of change a scan cares about, once the file is closed.
#[test]
fn every_closed_change_is_reported() {
    let t = tree();
    let path = |name: &str| t.root.join(name);
    for name in [
        "append", "rewrite", "touch", "chmod", "delete", "save", "held",
    ] {
        std::fs::write(path(name), b"0").unwrap();
    }
    std::fs::create_dir(path("folder")).unwrap();
    let _ = sentinel(&t.root, current_id());
    let since = current_id();

    append(&path("append"));
    let rewritten = std::fs::metadata(path("rewrite"))
        .unwrap()
        .modified()
        .unwrap();
    std::fs::write(path("rewrite"), b"1").unwrap();
    std::fs::File::options()
        .write(true)
        .open(path("rewrite"))
        .unwrap()
        .set_modified(rewritten)
        .unwrap();
    std::fs::File::options()
        .write(true)
        .open(path("touch"))
        .unwrap()
        .set_modified(std::time::UNIX_EPOCH)
        .unwrap();
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path("chmod"), std::fs::Permissions::from_mode(0o600)).unwrap();
    std::fs::remove_file(path("delete")).unwrap();
    std::fs::write(path(".tmp"), b"1").unwrap();
    std::fs::rename(path(".tmp"), path("save")).unwrap();
    std::fs::rename(path("folder"), path("folder2")).unwrap();

    let seen = sentinel(&t.root, since);
    for name in [
        "append", "rewrite", "touch", "chmod", "delete", "save", "folder", "folder2",
    ] {
        assert!(
            seen.paths.contains(&path(name)),
            "{name} not in {:?}",
            seen.paths
        );
    }
    assert!(
        !seen.paths.contains(&path("held")),
        "an untouched file is not reported"
    );
}

/// The call that should turn a time into an id answers 0 at every age, so
/// how far back the log reaches cannot be asked by date.
#[test]
fn an_id_cannot_be_looked_up_by_time() {
    let t = tree();
    let dev = std::fs::metadata(&t.root).unwrap().dev() as i32;
    let core_foundation_now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
        - 978_307_200.0;
    for days_ago in [0.0, 1.0, 30.0] {
        let id = unsafe {
            FSEventsGetLastEventIdForDeviceBeforeTime(dev, core_foundation_now - days_ago * 86400.0)
        };
        assert_eq!(id, 0, "{days_ago} days ago");
    }
}
