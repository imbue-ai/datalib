//! Shared filesystem-scanning primitives for file-backed providers.
//!
//! Factored out of `datalib-etl-fsindex`, which grew them first and is
//! still their heaviest user. Two providers now need the same three
//! things — hash a file's bytes, decide whether a previously-seen path
//! can skip that hash, and walk a tree honoring gitignore-shaped rules
//! — and duplicating them would let the two copies drift on exactly the
//! subtleties that are easy to get wrong (the mmap threshold, the
//! `nostamp` fallback, the `rescan` sentinel).
//!
//! What deliberately did NOT move here:
//!
//! - **Directory tree-hashing** (`hash_tree`). Only fsindex builds a
//!   Merkle tree over directories; a document provider hashes leaves and
//!   stops. It stays in fsindex next to the canonicalization doc that
//!   defines its wire format.
//! - **The `.fsindex.yaml` cascade and UUID stamping.** That is
//!   fsindex's opt-in upstream mutation, not a general scanning
//!   concern (see fsindex's `DOWNLOAD.md` §"Stamping policy").
//! - **The post-order streaming walker.** fsindex's walker is tuned for
//!   tens of millions of entries and folds child hashes into parents;
//!   [`walk_files`] here is a flat leaf-only walk for corpora three
//!   orders of magnitude smaller. Sharing one walker would mean
//!   carrying fsindex's post-order machinery into callers that have no
//!   directory rows to fold into.

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

/// Files at or above this size use `Hasher::update_mmap`; smaller files
/// stream via `update_reader`. blake3 upstream guidance: mmap wins for
/// large files because the kernel pages in lazily and one userspace
/// copy is avoided; for tiny files the mmap setup cost dominates. 16
/// MiB is the threshold blake3's own `b3sum` CLI uses.
const MMAP_THRESHOLD: u64 = 16 * 1024 * 1024;

/// Hash file bytes. Streams below [`MMAP_THRESHOLD`], mmaps above it.
pub fn hash_file(path: &Path, size: u64) -> Result<Blake3> {
    let mut hasher = blake3::Hasher::new();
    if size >= MMAP_THRESHOLD {
        hasher
            .update_mmap(path)
            .with_context(|| format!("mmap-hash {}", path.display()))?;
    } else {
        let f = File::open(path).with_context(|| format!("open for hash {}", path.display()))?;
        hasher
            .update_reader(f)
            .with_context(|| format!("hash {}", path.display()))?;
    }
    Ok(*hasher.finalize().as_bytes())
}

/// Hash a symlink's target bytes, so a retarget registers as a content
/// change.
pub fn hash_symlink_target(target: &[u8]) -> Blake3 {
    *blake3::hash(target).as_bytes()
}

// ─────────────────────────────────────────────────────────────────────
// Fast-rescan cursor (Unison's `dataClearlyUnchanged`)
// ─────────────────────────────────────────────────────────────────────

/// Which fields of the stat triple are trustworthy on the filesystem
/// this row was recorded from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StampKind {
    /// `(mtime, size, inode, dev)` all compared. The normal case.
    Inode,
    /// Inode is not stable here (some FUSE mounts, some network
    /// filesystems), so only `(mtime, size)` are compared. Less safe,
    /// but it is Unison's own behavior on those filesystems.
    NoStamp,
    /// "The previous run was interrupted mid-hash of this path." Forces
    /// a rehash regardless of what the triple says. Set before opening
    /// the file, cleared once the hash is durably written.
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

/// Reuse-vs-rehash for one previously-scanned path. Pure; no I/O.
///
/// Mirrors Unison's `dataClearlyUnchanged` (`src/fpcache.ml:243`). The
/// decision compares only against what was *stored*: the `stamp_kind`
/// the walker would assign to the new row is a platform decision made
/// elsewhere and does not enter here.
pub fn decide(prev: Option<&StampCursor>, fresh: &FreshStat) -> StampDecision {
    let Some(prev) = prev else {
        return StampDecision::Rehash;
    };
    if matches!(prev.stamp_kind, StampKind::Rescan) {
        return StampDecision::Rehash;
    }
    if prev.mtime_ns != fresh.mtime_ns || prev.size != fresh.size {
        return StampDecision::Rehash;
    }
    if matches!(prev.stamp_kind, StampKind::Inode)
        && (prev.inode != fresh.inode || prev.dev != fresh.dev)
    {
        return StampDecision::Rehash;
    }
    StampDecision::ReuseHash
}

/// Extract the rescan triple from a `Metadata`. On non-Unix the inode
/// and dev come back `None`, which callers should pair with
/// [`StampKind::NoStamp`].
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

// ─────────────────────────────────────────────────────────────────────
// Flat leaf walk
// ─────────────────────────────────────────────────────────────────────

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
pub struct WalkError {
    pub path: PathBuf,
    pub error: String,
}

/// Walk `root` and yield every regular file whose path satisfies
/// `accept`, honoring `.gitignore`-shaped rules found in the tree plus
/// any `extra_ignores` globs supplied by config.
///
/// Symlink policy: a symlink **to a file** is followed and indexed, a
/// symlink **to a directory** is not descended into. Descending is what
/// creates unbounded loops (`a/link -> ..`), whereas a link to a file
/// terminates immediately — and refusing those would mean skipping real
/// documents, including every input under a Bazel runfiles tree, which
/// is entirely symlinks.
pub fn walk_files<F>(
    root: &Path,
    extra_ignores: &[String],
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
        .parents(false);

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

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(stamp: StampKind, mtime: i64, size: i64, inode: Option<i64>) -> StampCursor {
        StampCursor {
            mtime_ns: mtime,
            size,
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

        let (files, errs) = walk_files(d.path(), &[], |p| {
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
        let (files, _) = walk_files(d.path(), &[], |p| {
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
        let (files, errors) = walk_files(d.path(), &[], |p| {
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

        let (files, _) = walk_files(d.path(), &["skipme/**".into()], |p| {
            p.extension().and_then(|e| e.to_str()) == Some("pdf")
        })
        .unwrap();
        let rels: Vec<&str> = files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, vec!["keep.pdf"]);
    }
}
