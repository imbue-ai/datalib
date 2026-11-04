//! Shared filesystem-scanning primitives for file-backed providers.

use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use ignore::gitignore::Gitignore;
use ignore::overrides::{Override, OverrideBuilder};
use ignore::Match;
use rayon::prelude::*;

/// The raw 32-byte blake3 digest. Stored as a BLOB; rendered as hex
/// only for human-facing output (test snapshots, ad-hoc `hex(blake3)`
/// queries). Hex would double the per-row hash bytes both in the table
/// and in its index, which is a real cost at fsindex's design scale.
pub type Blake3 = [u8; 32];

/// Render a digest as lowercase hex. For human-facing surfaces and for
/// providers (like `pdf`) that key rows on the hex form because the
/// digest doubles as a user-visible document identity.
pub fn to_hex(h: &Blake3) -> String {
    let mut s = String::with_capacity(64);
    for b in h {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// Files at or above this size are hashed on every core, read in
/// [`BIG_READ_CHUNK`] pieces so a caller can report how far along it is.
/// Below it the thread hand-off costs more than it saves.
const PARALLEL_THRESHOLD: u64 = 16 * 1024 * 1024;
const BIG_READ_CHUNK: usize = 8 * 1024 * 1024;

pub fn hash_file(path: &Path, size: u64) -> Result<Blake3> {
    hash_file_reporting(path, size, |_| {})
}

/// [`hash_file`], calling `on_bytes` with the running byte count after
/// each chunk of a large file (small files report once, when done).
pub fn hash_file_reporting(
    path: &Path,
    size: u64,
    mut on_bytes: impl FnMut(u64),
) -> Result<Blake3> {
    use std::io::Read as _;
    let mut hasher = blake3::Hasher::new();
    let mut f = File::open(path).with_context(|| format!("open for hash {}", path.display()))?;
    if size < PARALLEL_THRESHOLD {
        hasher
            .update_reader(f)
            .with_context(|| format!("hash {}", path.display()))?;
        on_bytes(size);
    } else {
        let mut buf = vec![0u8; BIG_READ_CHUNK];
        let mut done = 0u64;
        loop {
            let n = f
                .read(&mut buf)
                .with_context(|| format!("read {}", path.display()))?;
            if n == 0 {
                break;
            }
            hasher.update_rayon(&buf[..n]);
            done += n as u64;
            on_bytes(done);
        }
    }
    Ok(*hasher.finalize().as_bytes())
}

/// The hash of bytes already in hand, the same one [`hash_file`] gives.
pub fn hash_bytes(bytes: &[u8]) -> Blake3 {
    let mut hasher = blake3::Hasher::new();
    if bytes.len() as u64 >= PARALLEL_THRESHOLD {
        hasher.update_rayon(bytes);
    } else {
        hasher.update(bytes);
    }
    *hasher.finalize().as_bytes()
}

pub fn hash_symlink_target(target: &[u8]) -> Blake3 {
    *blake3::hash(target).as_bytes()
}

// Fast-rescan cursor (Unison's `dataClearlyUnchanged`)

/// Which fields of the stat triple are trustworthy on the filesystem
/// this row was recorded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StampKind {
    /// `(mtime, size, ctime, inode, dev)` all compared. The normal case.
    Inode,
    /// Inode is not stable here (some FUSE mounts, some network
    /// filesystems), so only `(mtime, size, ctime)` are compared.
    NoStamp,
    /// Forces a rehash regardless of what the triple says. Nothing
    /// writes it; a stored `stamp_kind` this build does not recognise
    /// reads as this ([`StampKind::from_str_or_rescan`]).
    Rescan,
}

impl StampKind {
    pub fn as_str(self) -> &'static str {
        match self {
            StampKind::Inode => "inode",
            StampKind::NoStamp => "nostamp",
            StampKind::Rescan => "rescan",
        }
    }

    pub fn from_str_or_rescan(s: &str) -> Self {
        match s {
            "inode" => StampKind::Inode,
            "nostamp" => StampKind::NoStamp,
            // An unrecognized value means a writer we don't understand
            // touched this row; rehashing is the safe reading.
            _ => StampKind::Rescan,
        }
    }
}

/// What we recorded for a path on a previous scan.
#[derive(Debug, Clone, Copy)]
pub struct StampCursor {
    pub mtime_ns: i64,
    pub size: i64,
    pub ctime_ns: Option<i64>,
    pub stamp_kind: StampKind,
    pub inode: Option<i64>,
    pub dev: Option<i64>,
}

/// A fresh stat of the same path, taken this scan.
#[derive(Debug, Clone, Copy)]
pub struct FreshStat {
    pub mtime_ns: i64,
    pub size: i64,
    pub inode: Option<i64>,
    pub dev: Option<i64>,
    pub ctime_ns: Option<i64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StampDecision {
    ReuseHash,
    Rehash,
}

pub fn decide(prev: Option<&StampCursor>, fresh: &FreshStat) -> StampDecision {
    let Some(prev) = prev else {
        return StampDecision::Rehash;
    };
    if matches!(prev.stamp_kind, StampKind::Rescan) {
        return StampDecision::Rehash;
    }
    // ctime moves on every write and nothing can set it back, so it
    // catches a rewrite whose mtime was restored (`cp -p`, `touch -r`, a
    // restore tool) that the mtime alone would miss.
    if prev.mtime_ns != fresh.mtime_ns || prev.size != fresh.size || prev.ctime_ns != fresh.ctime_ns
    {
        return StampDecision::Rehash;
    }
    if matches!(prev.stamp_kind, StampKind::Inode)
        && (prev.inode != fresh.inode || prev.dev != fresh.dev)
    {
        return StampDecision::Rehash;
    }
    StampDecision::ReuseHash
}

pub fn fresh_stat(md: &std::fs::Metadata) -> FreshStat {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        FreshStat {
            mtime_ns: md.mtime() * 1_000_000_000 + i64::from(md.mtime_nsec() as i32),
            size: md.size() as i64,
            inode: Some(md.ino() as i64),
            dev: Some(md.dev() as i64),
            ctime_ns: Some(md.ctime() * 1_000_000_000 + i64::from(md.ctime_nsec() as i32)),
        }
    }
    #[cfg(not(unix))]
    {
        let mtime_ns = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_nanos() as i64)
            .unwrap_or(0);
        FreshStat {
            mtime_ns,
            size: md.len() as i64,
            inode: None,
            dev: None,
            ctime_ns: None,
        }
    }
}

/// The `stamp_kind` to record for a fresh stat: `Inode` when the
/// platform gave us an inode to compare next time, `NoStamp` otherwise.
pub fn stamp_kind_for(fresh: &FreshStat) -> StampKind {
    if fresh.inode.is_some() {
        StampKind::Inode
    } else {
        StampKind::NoStamp
    }
}

// Flat leaf walk

/// One visited file.
pub struct WalkedFile {
    /// Absolute path on disk.
    pub path: PathBuf,
    /// Path relative to the scan root, slash-separated. This is the
    /// stable id callers key rows on — absolute paths move when the
    /// data root moves.
    pub rel: String,
    pub stat: FreshStat,
    /// The file has no blocks allocated here, which is how a cloud placeholder
    /// looks from a stat. Says nothing about a file with no bytes to hold.
    pub no_blocks: bool,
}

/// One entry we could not read. Surfaced rather than swallowed so the
/// caller can land it in a `_bookkeeping` sidecar per the framework's
/// universal pattern.
#[derive(Debug, Clone)]
pub struct WalkError {
    pub path: PathBuf,
    pub error: String,
}

/// What a folder's entry is, as far as listing it says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    Dir,
    File,
    /// A symlink, or something a listing does not name, which a stat of the
    /// path settles.
    Other,
}

struct ListedEntry {
    name: std::ffi::OsString,
    kind: EntryKind,
    /// The file's stat, from the listing where the platform hands it over.
    stat: Option<(FreshStat, bool)>,
}

/// The ignore files in force at one folder, each link one folder's own.
struct IgnoreRules {
    parent: Option<std::sync::Arc<IgnoreRules>>,
    dot_ignore: Gitignore,
    git_ignore: Gitignore,
    /// The folder holds a `.git`: `.gitignore` files count only inside a
    /// repository, and rules above the repository's top do not reach into it.
    has_git: bool,
}

impl IgnoreRules {
    fn chain(&self) -> impl Iterator<Item = &IgnoreRules> {
        std::iter::successors(Some(self), |r| r.parent.as_deref())
    }

    fn ignores(&self, path: &Path, is_dir: bool) -> bool {
        let any_git = self.chain().any(|r| r.has_git);
        let (mut dot, mut git) = (Match::None, Match::None);
        let mut saw_git = false;
        for rules in self.chain() {
            if dot.is_none() {
                dot = rules.dot_ignore.matched(path, is_dir).map(|_| ());
            }
            if any_git && !saw_git && git.is_none() {
                git = rules.git_ignore.matched(path, is_dir).map(|_| ());
            }
            saw_git |= rules.has_git;
        }
        if dot.is_none() { git } else { dot }.is_ignore()
    }

    fn beneath(
        parent: Option<std::sync::Arc<IgnoreRules>>,
        dir: &Path,
        entries: &[ListedEntry],
    ) -> std::sync::Arc<IgnoreRules> {
        let named = |name: &str| entries.iter().any(|e| e.name == name);
        let (git_ignore, dot_ignore, has_git) =
            (named(".gitignore"), named(".ignore"), named(".git"));
        match parent {
            Some(parent) if !git_ignore && !dot_ignore && !has_git => parent,
            parent => {
                let rules = |present: bool, name: &str| {
                    if !present {
                        return Gitignore::empty();
                    }
                    Gitignore::new(dir.join(name)).0
                };
                std::sync::Arc::new(IgnoreRules {
                    parent,
                    dot_ignore: rules(dot_ignore, ".ignore"),
                    git_ignore: rules(git_ignore, ".gitignore"),
                    has_git,
                })
            }
        }
    }
}

struct Walker<'a, F> {
    root: &'a Path,
    max_depth: Option<usize>,
    overrides: Override,
    accept: F,
}

#[derive(Default)]
struct Found {
    files: Vec<WalkedFile>,
    errors: Vec<WalkError>,
}

impl Found {
    fn merge(mut self, other: Found) -> Found {
        self.files.extend(other.files);
        self.errors.extend(other.errors);
        self
    }
}

impl<F: Fn(&Path) -> bool + Sync> Walker<'_, F> {
    fn walk(&self, dir: &Path, depth: usize, parent: Option<std::sync::Arc<IgnoreRules>>) -> Found {
        let entries = match list_dir(dir) {
            Ok(entries) => entries,
            Err(e) => {
                return Found {
                    files: Vec::new(),
                    errors: vec![WalkError {
                        path: dir.to_path_buf(),
                        error: e.to_string(),
                    }],
                }
            }
        };
        let rules = IgnoreRules::beneath(parent, dir, &entries);
        let (dirs, files): (Vec<_>, Vec<_>) =
            entries.into_iter().partition(|e| e.kind == EntryKind::Dir);
        let found = files
            .par_iter()
            .with_min_len(STATS_PER_TASK)
            .fold(Found::default, |mut found, entry| {
                self.visit_file(dir, entry, &rules, &mut found);
                found
            })
            .reduce(Found::default, Found::merge);
        if self.max_depth.is_some_and(|max| depth + 1 >= max) {
            return found;
        }
        dirs.par_iter()
            .map(|entry| {
                let path = dir.join(&entry.name);
                if self.skips(&path, true, &rules) {
                    return Found::default();
                }
                self.walk(&path, depth + 1, Some(rules.clone()))
            })
            .reduce(Found::default, Found::merge)
            .merge(found)
    }

    fn skips(&self, path: &Path, is_dir: bool, rules: &IgnoreRules) -> bool {
        match self.overrides.matched(path, is_dir) {
            Match::Ignore(_) => true,
            Match::Whitelist(_) => false,
            Match::None => rules.ignores(path, is_dir),
        }
    }

    fn visit_file(&self, dir: &Path, entry: &ListedEntry, rules: &IgnoreRules, found: &mut Found) {
        let path = dir.join(&entry.name);
        if self.skips(&path, false, rules) || !(self.accept)(&path) {
            return;
        }
        // Not traversing the link would hand over the link's own stat, so a
        // symlink is resolved here. A dangling link, or one pointing at a
        // directory, is not a file we can hash.
        let (stat, no_blocks) = match entry.stat {
            Some(stat) => stat,
            None => match std::fs::metadata(&path) {
                Ok(m) if m.is_file() => (fresh_stat(&m), has_no_blocks(&m)),
                Ok(_) => return,
                Err(e) => {
                    found.errors.push(WalkError {
                        path,
                        error: e.to_string(),
                    });
                    return;
                }
            },
        };
        let rel = path
            .strip_prefix(self.root)
            .unwrap_or(&path)
            .components()
            .map(|c| c.as_os_str().to_string_lossy())
            .collect::<Vec<_>>()
            .join("/");
        found.files.push(WalkedFile {
            path,
            rel,
            stat,
            no_blocks,
        });
    }
}

/// Files a task stats between handing work to another core: below this the
/// hand-off costs more than the stats.
const STATS_PER_TASK: usize = 64;

/// `max_depth: Some(1)` walks `root`'s own entries and opens no folder
/// beneath it. Every folder is listed on every core, and a file's stat comes
/// with its folder's listing where the platform can do that (macOS).
///
/// Follows what a person's `.gitignore` and `.ignore` files say, as git would:
/// a `.gitignore` counts only inside a repository. Dotfiles are walked, since a
/// corpus can legitimately live in one. `extra_ignores` are gitignore-shaped
/// and win over those files. A symlink to a file is that file; a symlink to a
/// folder is not followed.
pub fn walk_files<F>(
    root: &Path,
    extra_ignores: &[String],
    max_depth: Option<usize>,
    accept: F,
) -> Result<(Vec<WalkedFile>, Vec<WalkError>)>
where
    F: Fn(&Path) -> bool + Sync,
{
    let mut overrides = OverrideBuilder::new(root);
    for pat in extra_ignores {
        // Override syntax is inverted relative to gitignore: a bare glob
        // *whitelists*. Prefix with `!` so a config `ignore` entry reads the
        // way a user expects.
        overrides
            .add(&format!("!{pat}"))
            .with_context(|| format!("bad ignore pattern {pat:?}"))?;
    }
    let walker = Walker {
        root,
        max_depth,
        overrides: overrides.build().context("build ignore overrides")?,
        accept,
    };
    let Found { mut files, errors } = walker.walk(root, 0, None);
    // Deterministic order so two scans of an unchanged tree produce
    // identical row ordering (and therefore identical dolt diffs).
    files.par_sort_unstable_by(|a, b| a.rel.cmp(&b.rel));
    Ok((files, errors))
}

fn has_no_blocks(md: &std::fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        md.blocks() == 0
    }
    #[cfg(not(unix))]
    {
        let _ = md;
        false
    }
}

/// A folder's entries, the plain way: one call for the names, and each
/// file's stat left to a call of its own.
fn read_dir_entries(dir: &Path) -> std::io::Result<Vec<ListedEntry>> {
    std::fs::read_dir(dir)?
        .map(|entry| {
            let entry = entry?;
            let kind = entry.file_type().map_or(EntryKind::Other, |ft| {
                if ft.is_dir() {
                    EntryKind::Dir
                } else if ft.is_file() {
                    EntryKind::File
                } else {
                    EntryKind::Other
                }
            });
            Ok(ListedEntry {
                name: entry.file_name(),
                kind,
                stat: None,
            })
        })
        .collect()
}

#[cfg(not(target_os = "macos"))]
fn list_dir(dir: &Path) -> std::io::Result<Vec<ListedEntry>> {
    read_dir_entries(dir)
}

#[cfg(target_os = "macos")]
fn list_dir(dir: &Path) -> std::io::Result<Vec<ListedEntry>> {
    bulk::list_dir(dir)
}

/// `getattrlistbulk`: one folder's names and every entry's stat in one call,
/// about half the time of a listing and a stat per file.
#[cfg(target_os = "macos")]
mod bulk {
    use super::{read_dir_entries, EntryKind, FreshStat, ListedEntry};
    use std::ffi::{CString, OsString};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::path::Path;

    const BUFFER_BYTES: usize = 256 * 1024;
    const VREG: u32 = 1;
    const VDIR: u32 = 2;

    fn at<T: Copy>(entry: &[u8], offset: usize) -> T {
        assert!(offset + std::mem::size_of::<T>() <= entry.len());
        // Safety: in bounds by the assert, and `T` is a plain integer.
        unsafe { std::ptr::read_unaligned(entry[offset..].as_ptr().cast()) }
    }

    fn nanos(entry: &[u8], offset: usize) -> i64 {
        let secs: i64 = at(entry, offset);
        let nsec: i64 = at(entry, offset + 8);
        secs * 1_000_000_000 + i64::from(nsec as i32)
    }

    /// One entry of the buffer. The attributes sit in the order of their bits,
    /// after the set of those returned, and one not returned takes no room.
    fn parse(entry: &[u8]) -> Option<ListedEntry> {
        let common: u32 = at(entry, 4);
        let file: u32 = at(entry, 16);
        let mut q = 4 + std::mem::size_of::<libc::attribute_set_t>();
        let needed = libc::ATTR_CMN_NAME
            | libc::ATTR_CMN_DEVID
            | libc::ATTR_CMN_OBJTYPE
            | libc::ATTR_CMN_MODTIME
            | libc::ATTR_CMN_CHGTIME
            | libc::ATTR_CMN_FILEID;
        if common & libc::ATTR_CMN_NAME == 0 {
            return None;
        }
        let name_at = q as i64 + i64::from(at::<i32>(entry, q));
        let name_len: u32 = at(entry, q + 4);
        let name = entry.get(name_at as usize..name_at as usize + name_len as usize - 1)?;
        let name = OsString::from_vec(name.to_vec());
        q += 8;
        if common & needed != needed {
            return Some(ListedEntry {
                name,
                kind: EntryKind::Other,
                stat: None,
            });
        }
        let dev: i32 = at(entry, q);
        q += 4;
        let objtype: u32 = at(entry, q);
        q += 4;
        let mtime_ns = nanos(entry, q);
        q += 16;
        let ctime_ns = nanos(entry, q);
        q += 16;
        let inode: u64 = at(entry, q);
        q += 8;
        let (mut allocated, mut size) = (None, None);
        if file & libc::ATTR_FILE_ALLOCSIZE != 0 {
            allocated = Some(at::<i64>(entry, q));
            q += 8;
        }
        if file & libc::ATTR_FILE_DATALENGTH != 0 {
            size = Some(at::<i64>(entry, q));
        }
        let kind = match objtype {
            VDIR => EntryKind::Dir,
            VREG => EntryKind::File,
            _ => EntryKind::Other,
        };
        let stat = match (kind, allocated, size) {
            (EntryKind::File, Some(allocated), Some(size)) => Some((
                FreshStat {
                    mtime_ns,
                    size,
                    inode: Some(inode as i64),
                    dev: Some(i64::from(dev)),
                    ctime_ns: Some(ctime_ns),
                },
                allocated == 0,
            )),
            _ => None,
        };
        Some(ListedEntry { name, kind, stat })
    }

    pub(super) fn list_dir(dir: &Path) -> std::io::Result<Vec<ListedEntry>> {
        let path = CString::new(dir.as_os_str().as_bytes())?;
        // Safety: `path` is a valid C string.
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_DIRECTORY) };
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // Safety: an all-zero `attrlist` is a valid one asking for nothing.
        let mut asked: libc::attrlist = unsafe { std::mem::zeroed() };
        asked.bitmapcount = libc::ATTR_BIT_MAP_COUNT;
        asked.commonattr = libc::ATTR_CMN_RETURNED_ATTRS
            | libc::ATTR_CMN_NAME
            | libc::ATTR_CMN_DEVID
            | libc::ATTR_CMN_OBJTYPE
            | libc::ATTR_CMN_MODTIME
            | libc::ATTR_CMN_CHGTIME
            | libc::ATTR_CMN_FILEID;
        asked.fileattr = libc::ATTR_FILE_ALLOCSIZE | libc::ATTR_FILE_DATALENGTH;
        let mut buffer = vec![0u8; BUFFER_BYTES];
        let mut out = Vec::new();
        let result = loop {
            // Safety: `fd` is open, `asked` outlives the call, and `buffer` is
            // writable for the length given.
            let count = unsafe {
                libc::getattrlistbulk(
                    fd,
                    std::ptr::from_mut(&mut asked).cast(),
                    buffer.as_mut_ptr().cast(),
                    buffer.len(),
                    0,
                )
            };
            if count < 0 {
                break Err(std::io::Error::last_os_error());
            }
            if count == 0 {
                break Ok(());
            }
            let mut offset = 0usize;
            for _ in 0..count {
                let length: u32 = at(&buffer, offset);
                let entry = &buffer[offset..offset + length as usize];
                out.extend(parse(entry));
                offset += length as usize;
            }
        };
        // Safety: `fd` is open and this is its only close.
        unsafe { libc::close(fd) };
        match result {
            Ok(()) => Ok(out),
            // A filesystem may not do this at all. It opened, so the plain way
            // says either the same thing or why it cannot.
            Err(e) => {
                tracing::warn!(path = %dir.display(), error = %e, "fswalk_bulk_listing_failed");
                read_dir_entries(dir)
            }
        }
    }
}

/// Rewrite `path` to `body` in place and put its mtime back, as `cp -p`
/// over an existing file does. Rewrites again until the change time
/// has moved, which a coarse filesystem clock can take a tick to show.
#[cfg(all(test, unix))]
pub(crate) fn rewrite_keeping_mtime(path: &Path, body: &[u8]) {
    use std::os::unix::fs::MetadataExt;
    let before = std::fs::metadata(path).unwrap();
    let stamp = |m: &std::fs::Metadata| (m.ino(), m.len(), m.mtime(), m.mtime_nsec());
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    loop {
        std::fs::write(path, body).unwrap();
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(before.modified().unwrap())
            .unwrap();
        let after = std::fs::metadata(path).unwrap();
        assert_eq!(
            stamp(&after),
            stamp(&before),
            "only the bytes and ctime move"
        );
        if (after.ctime(), after.ctime_nsec()) != (before.ctime(), before.ctime_nsec()) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the change time of {} never moved",
            path.display()
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(stamp: StampKind, mtime: i64, size: i64, inode: Option<i64>) -> StampCursor {
        StampCursor {
            mtime_ns: mtime,
            size,
            ctime_ns: None,
            stamp_kind: stamp,
            inode,
            dev: Some(0),
        }
    }
    fn stat(mtime: i64, size: i64, inode: Option<i64>) -> FreshStat {
        FreshStat {
            mtime_ns: mtime,
            size,
            inode,
            dev: Some(0),
            ctime_ns: None,
        }
    }

    #[test]
    fn no_prev_means_rehash() {
        assert_eq!(decide(None, &stat(1, 1, None)), StampDecision::Rehash);
    }

    #[test]
    fn rescan_kind_forces_rehash_even_when_triple_matches() {
        let p = cursor(StampKind::Rescan, 1, 1, Some(7));
        assert_eq!(
            decide(Some(&p), &stat(1, 1, Some(7))),
            StampDecision::Rehash
        );
    }

    #[test]
    fn inode_match_reuses() {
        let p = cursor(StampKind::Inode, 1, 1, Some(7));
        assert_eq!(
            decide(Some(&p), &stat(1, 1, Some(7))),
            StampDecision::ReuseHash
        );
    }

    #[test]
    fn inode_mismatch_rehashes() {
        let p = cursor(StampKind::Inode, 1, 1, Some(7));
        assert_eq!(
            decide(Some(&p), &stat(1, 1, Some(8))),
            StampDecision::Rehash
        );
    }

    #[test]
    fn nostamp_ignores_inode() {
        let p = cursor(StampKind::NoStamp, 1, 1, None);
        assert_eq!(
            decide(Some(&p), &stat(1, 1, Some(99))),
            StampDecision::ReuseHash
        );
    }

    #[test]
    fn size_change_rehashes_even_when_mtime_is_identical() {
        let p = cursor(StampKind::Inode, 5, 100, Some(7));
        assert_eq!(
            decide(Some(&p), &stat(5, 101, Some(7))),
            StampDecision::Rehash
        );
    }

    /// A file rewritten in place to the same size, its mtime put back:
    /// only ctime says it changed.
    #[test]
    fn a_ctime_change_rehashes_when_everything_else_matches() {
        let p = StampCursor {
            ctime_ns: Some(10),
            ..cursor(StampKind::Inode, 5, 100, Some(7))
        };
        let fresh = FreshStat {
            ctime_ns: Some(11),
            ..stat(5, 100, Some(7))
        };
        assert_eq!(decide(Some(&p), &fresh), StampDecision::Rehash);
    }

    #[test]
    fn unknown_stamp_kind_string_reads_as_rescan() {
        assert_eq!(
            StampKind::from_str_or_rescan("something-new"),
            StampKind::Rescan
        );
    }

    #[test]
    fn hex_is_lowercase_and_64_chars() {
        let h: Blake3 = [0xab; 32];
        let s = to_hex(&h);
        assert_eq!(s.len(), 64);
        assert!(s
            .chars()
            .all(|c| c.is_ascii_hexdigit() && !c.is_uppercase()));
    }

    #[test]
    fn walk_finds_accepted_files_and_skips_others() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("sub/deep")).unwrap();
        std::fs::write(d.path().join("a.pdf"), b"x").unwrap();
        std::fs::write(d.path().join("sub/b.pdf"), b"y").unwrap();
        std::fs::write(d.path().join("sub/deep/c.txt"), b"z").unwrap();

        let (files, errs) = walk_files(d.path(), &[], None, |p| {
            p.extension().and_then(|e| e.to_str()) == Some("pdf")
        })
        .unwrap();
        assert!(errs.is_empty());
        let rels: Vec<&str> = files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, vec!["a.pdf", "sub/b.pdf"]);
    }

    #[test]
    fn symlinks_to_files_are_indexed_but_dir_links_are_not_followed() {
        // Bazel runfiles trees are entirely symlinks, so refusing them
        // would silently empty every hermetic test's input.
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("real")).unwrap();
        std::fs::write(d.path().join("real/a.pdf"), b"x").unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(d.path().join("real/a.pdf"), d.path().join("link.pdf"))
                .unwrap();
            // A directory link that would otherwise recurse forever.
            std::os::unix::fs::symlink(d.path(), d.path().join("loop")).unwrap();
        }
        let (files, _) = walk_files(d.path(), &[], None, |p| {
            p.extension().and_then(|e| e.to_str()) == Some("pdf")
        })
        .unwrap();
        let rels: Vec<&str> = files.iter().map(|f| f.rel.as_str()).collect();
        #[cfg(unix)]
        assert_eq!(rels, vec!["link.pdf", "real/a.pdf"]);
        #[cfg(not(unix))]
        assert_eq!(rels, vec!["real/a.pdf"]);
    }

    #[test]
    fn dangling_symlink_is_reported_not_indexed() {
        let d = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(d.path().join("nope.pdf"), d.path().join("dead.pdf")).unwrap();
        let (files, errors) = walk_files(d.path(), &[], None, |p| {
            p.extension().and_then(|e| e.to_str()) == Some("pdf")
        })
        .unwrap();
        assert!(files.is_empty());
        #[cfg(unix)]
        assert_eq!(errors.len(), 1, "a broken link should surface, not vanish");
        #[cfg(not(unix))]
        let _ = errors;
    }

    #[test]
    fn extra_ignore_patterns_prune_subtrees() {
        let d = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(d.path().join("skipme")).unwrap();
        std::fs::write(d.path().join("keep.pdf"), b"x").unwrap();
        std::fs::write(d.path().join("skipme/no.pdf"), b"y").unwrap();

        let (files, _) = walk_files(d.path(), &["skipme/**".into()], None, |p| {
            p.extension().and_then(|e| e.to_str()) == Some("pdf")
        })
        .unwrap();
        let rels: Vec<&str> = files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, vec!["keep.pdf"]);
    }

    /// What `ignore`'s own walker makes of the same options: the semantics
    /// [`walk_files`] promises, from the crate whose rules it follows.
    fn what_the_ignore_crate_walks(root: &Path, extra: &[String]) -> Vec<String> {
        let mut builder = ignore::WalkBuilder::new(root);
        builder
            .follow_links(false)
            .hidden(false)
            .git_ignore(true)
            .git_global(false)
            .git_exclude(false)
            .parents(false);
        if !extra.is_empty() {
            let mut ov = OverrideBuilder::new(root);
            for pat in extra {
                ov.add(&format!("!{pat}")).unwrap();
            }
            builder.overrides(ov.build().unwrap());
        }
        let mut rels: Vec<String> = builder
            .build()
            .flatten()
            .filter(|e| e.path() != root)
            .filter(|e| std::fs::metadata(e.path()).is_ok_and(|m| m.is_file()))
            .map(|e| {
                e.path()
                    .strip_prefix(root)
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        rels.sort();
        rels
    }

    fn write_tree(root: &Path, files: &[(&str, &str)]) {
        for (rel, body) in files {
            let p = root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, body).unwrap();
        }
    }

    /// The walk reads ignore files itself, so it is held to the `ignore`
    /// crate's answer on the rules that have bitten: a `.gitignore` outside a
    /// repository counts for nothing, nested rules and `!` re-includes, a
    /// repository inside a repository, `.ignore`, a pruned folder.
    #[test]
    fn the_walk_ignores_what_the_ignore_crate_ignores() {
        let trees: &[&[(&str, &str)]] = &[
            // No repository: `.gitignore` is not in force, `.ignore` is.
            &[
                (".gitignore", "a.txt\n"),
                (".ignore", "b.txt\n"),
                ("a.txt", ""),
                ("b.txt", ""),
                ("sub/a.txt", ""),
                ("sub/b.txt", ""),
            ],
            // A repository: nested rules, a re-include, a folder pattern.
            &[
                (".git/HEAD", "ref"),
                (".gitignore", "*.log\nbuild/\n/rooted.txt\n"),
                ("keep.txt", ""),
                ("x.log", ""),
                ("rooted.txt", ""),
                ("sub/rooted.txt", ""),
                ("build/out.txt", ""),
                ("sub/build/out.txt", ""),
                ("sub/.gitignore", "!important.log\nlocal.txt\n"),
                ("sub/important.log", ""),
                ("sub/other.log", ""),
                ("sub/local.txt", ""),
                ("sub/deep/local.txt", ""),
                ("elsewhere/local.txt", ""),
            ],
            // A repository below the root: its rules stop at its top, and
            // the outer repository's do not reach in.
            &[
                (".git/HEAD", "ref"),
                (".gitignore", "*.outer\n"),
                ("a.outer", ""),
                ("inner/.git/HEAD", "ref"),
                ("inner/.gitignore", "*.inner\n"),
                ("inner/a.outer", ""),
                ("inner/a.inner", ""),
                ("b.inner", ""),
            ],
            // A repository below a folder with no repository above it.
            &[
                (".gitignore", "*.top\n"),
                ("a.top", ""),
                ("repo/.git/HEAD", "ref"),
                ("repo/.gitignore", "*.r\n"),
                ("repo/a.top", ""),
                ("repo/a.r", ""),
                ("repo/sub/a.r", ""),
                ("plain/a.r", ""),
            ],
            // `.ignore` above `.gitignore`, and one re-including the other.
            &[
                (".git/HEAD", "ref"),
                (".gitignore", "*.a\n"),
                (".ignore", "!keep.a\n*.b\n"),
                ("keep.a", ""),
                ("drop.a", ""),
                ("x.b", ""),
            ],
        ];
        for (i, files) in trees.iter().enumerate() {
            let d = tempfile::tempdir().unwrap();
            write_tree(d.path(), files);
            for extra in [
                vec![],
                vec!["*.txt".to_string()],
                vec!["sub/**".to_string()],
            ] {
                let (walked, errors) = walk_files(d.path(), &extra, None, |_| true).unwrap();
                assert!(errors.is_empty(), "{errors:?}");
                let rels: Vec<String> = walked.into_iter().map(|f| f.rel).collect();
                assert_eq!(
                    rels,
                    what_the_ignore_crate_walks(d.path(), &extra),
                    "tree {i} {extra:?}"
                );
            }
        }
    }

    /// A stat that came with a listing must be the stat a stat call gives, to
    /// the nanosecond: the host cache was written with these, and one field
    /// off would re-hash every file on the machine.
    #[test]
    fn a_stat_from_the_listing_is_the_stat() {
        let d = tempfile::tempdir().unwrap();
        write_tree(
            d.path(),
            &[
                ("a.txt", "aaa"),
                ("empty", ""),
                ("sub/\u{e9}t\u{e9}.txt", "b"),
            ],
        );
        std::fs::write(d.path().join("big"), vec![7u8; 3_000_000]).unwrap();
        let sparse = std::fs::File::create(d.path().join("sparse")).unwrap();
        sparse.set_len(1 << 30).unwrap();
        #[cfg(unix)]
        {
            std::fs::hard_link(d.path().join("a.txt"), d.path().join("hard")).unwrap();
            std::os::unix::fs::symlink(d.path().join("big"), d.path().join("link")).unwrap();
        }
        let (walked, errors) = walk_files(d.path(), &[], None, |_| true).unwrap();
        assert!(errors.is_empty(), "{errors:?}");
        assert!(walked.len() >= 5);
        for f in &walked {
            let m = std::fs::metadata(&f.path).unwrap();
            let s = fresh_stat(&m);
            assert_eq!(
                (
                    f.stat.mtime_ns,
                    f.stat.size,
                    f.stat.inode,
                    f.stat.dev,
                    f.stat.ctime_ns
                ),
                (s.mtime_ns, s.size, s.inode, s.dev, s.ctime_ns),
                "{}",
                f.rel
            );
            assert_eq!(f.no_blocks, has_no_blocks(&m), "{}", f.rel);
        }
    }

    /// A folder that will not list names itself, not the root, so what the
    /// walk cannot see is only what is under it.
    #[cfg(unix)]
    #[test]
    fn a_folder_that_will_not_list_is_the_error() {
        use std::os::unix::fs::PermissionsExt;
        let d = tempfile::tempdir().unwrap();
        write_tree(d.path(), &[("ok/a.txt", ""), ("locked/b.txt", "")]);
        let locked = d.path().join("locked");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read_dir(&locked).is_ok() {
            // Root reads through any mode; CI's container runs as root.
            std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
            return;
        }
        let (walked, errors) = walk_files(d.path(), &[], None, |_| true).unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert_eq!(walked.len(), 1);
        assert_eq!(walked[0].rel, "ok/a.txt");
        assert_eq!(errors.len(), 1);
        assert_eq!(errors[0].path, locked);
    }
}
