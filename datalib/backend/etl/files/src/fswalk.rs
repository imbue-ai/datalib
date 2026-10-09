//! Shared filesystem-scanning primitives for file-backed providers.

use std::fs::File;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

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
    pub meta: std::fs::Metadata,
}

/// One entry we could not read. Surfaced rather than swallowed so the
/// caller can land it in a `_bookkeeping` sidecar per the framework's
/// universal pattern.
#[derive(Debug, Clone)]
pub struct WalkError {
    pub path: PathBuf,
    pub error: String,
}

/// `max_depth: Some(1)` walks `root`'s own entries and opens no folder
/// beneath it.
pub fn walk_files<F>(
    root: &Path,
    extra_ignores: &[String],
    max_depth: Option<usize>,
    accept: F,
) -> Result<(Vec<WalkedFile>, Vec<WalkError>)>
where
    F: Fn(&Path) -> bool,
{
    let mut builder = ignore::WalkBuilder::new(root);
    builder
        .follow_links(false)
        .hidden(false) // index dotfiles; a corpus can legitimately live in one
        .git_ignore(true)
        .git_global(false)
        .git_exclude(false)
        .parents(false)
        .max_depth(max_depth);

    if !extra_ignores.is_empty() {
        let mut ov = ignore::overrides::OverrideBuilder::new(root);
        for pat in extra_ignores {
            // `ignore`'s override syntax is inverted relative to
            // gitignore: a bare glob *whitelists*. Prefix with `!` so a
            // config `ignore` entry reads the way a user expects.
            ov.add(&format!("!{pat}"))
                .with_context(|| format!("bad ignore pattern {pat:?}"))?;
        }
        builder.overrides(ov.build().context("build ignore overrides")?);
    }

    let mut files = Vec::new();
    let mut errors = Vec::new();
    for res in builder.build() {
        match res {
            Ok(entry) => {
                let ft = match entry.file_type() {
                    Some(ft) => ft,
                    // Only the root sentinel has no file type.
                    None => continue,
                };
                if ft.is_dir() {
                    continue;
                }
                let path = entry.path();
                if !accept(path) {
                    continue;
                }
                // `entry.metadata()` does not traverse the link when
                // `follow_links(false)`, so resolve it ourselves. A
                // dangling link, or one pointing at a directory, is not
                // a file we can hash.
                let meta = match std::fs::metadata(path) {
                    Ok(m) if m.is_file() => m,
                    Ok(_) => continue,
                    Err(e) => {
                        errors.push(WalkError {
                            path: path.to_path_buf(),
                            error: e.to_string(),
                        });
                        continue;
                    }
                };
                let rel = path
                    .strip_prefix(root)
                    .unwrap_or(path)
                    .components()
                    .map(|c| c.as_os_str().to_string_lossy())
                    .collect::<Vec<_>>()
                    .join("/");
                files.push(WalkedFile {
                    path: path.to_path_buf(),
                    rel,
                    meta,
                });
            }
            Err(e) => errors.push(WalkError {
                path: root.to_path_buf(),
                error: e.to_string(),
            }),
        }
    }
    // Deterministic order so two scans of an unchanged tree produce
    // identical row ordering (and therefore identical dolt diffs).
    files.sort_by(|a, b| a.rel.cmp(&b.rel));
    Ok((files, errors))
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
}
