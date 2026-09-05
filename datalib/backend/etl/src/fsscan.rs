//! "What changed on the filesystem since I last looked?"
//!
//! One walk, answered by joining two halves that live in different
//! places for good reasons:
//!
//! - **The host-wide fingerprint cache** ([`crate::fingerprint_cache`])
//!   — `abs_path → (stat, blake3)`. Expensive to compute, identical for
//!   every consumer, shared across scans, branches and providers, and
//!   deliberately unversioned because it describes a machine rather
//!   than a history.
//! - **The caller's own [`Watermark`]** — what *this* source has
//!   already ingested, which lives in that source's own store beside
//!   its other ingestion state.
//!
//! The cache cannot answer the question alone, and that is not a
//! limitation to be fixed: it is *shared*, so another consumer's scan
//! moves it. "Since I last looked" is only well-posed relative to a
//! particular looker.
//!
//! ```text
//!     scan(cache, root, opts, accept)   →  Scan     "what is there now"
//!     scan.changes_since(&watermark)    →  Changes  "what you have not dealt with"
//!     scan.watermark()                  →  Watermark  persist this
//! ```
//!
//! The walk itself hashes only what the cache cannot vouch for, so a
//! second provider scanning a tree the first one already walked pays
//! stat calls and nothing else.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

use crate::fingerprint_cache::{abs_key, EntryKind, Fingerprint, FingerprintCache};
use crate::fswalk::{self, Blake3, StampDecision, WalkError};

/// One file as this scan found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScannedFile {
    /// Absolute path on disk.
    pub path: PathBuf,
    /// Path relative to the scan root, slash-separated. The stable id
    /// callers key rows on — absolute paths move when a data root does.
    pub rel: String,
    pub size: i64,
    pub blake3: Blake3,
}

/// What a source has already dealt with: root-relative path → the
/// digest it ingested.
///
/// Built from whatever that source already stores. Nothing here needs
/// a new table: `pdf` and `media` keep `path → blake3` as a location
/// index anyway, and a provider keyed only by content can hand back an
/// empty map and rely on [`Changes::added`] plus its own
/// "do I have this digest?" check.
pub type Watermark = BTreeMap<String, Blake3>;

#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    /// Extra gitignore-shaped globs from config, on top of any
    /// `.gitignore` found in the tree.
    pub ignore: Vec<String>,
    /// Skip files larger than this rather than hashing them.
    pub max_bytes: Option<u64>,
    /// Ignore the cache and re-read every file. For
    /// `--reset-and-redownload`, and for proving the cache honest.
    pub force_rehash: bool,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanStats {
    /// Files whose digest came from the cache — **no content read**.
    pub reused: usize,
    /// Files actually opened and hashed.
    pub hashed: usize,
    pub bytes_hashed: u64,
    /// Bytes not read because the cache vouched for them.
    pub bytes_reused: u64,
    /// Files skipped for exceeding [`ScanOptions::max_bytes`].
    pub too_large: usize,
    /// Entries this host had already cached under the root.
    pub cache_read: usize,
    /// Fingerprints written back.
    pub cache_written: u64,
}

/// One walk's worth of "what is there now".
#[derive(Debug)]
pub struct Scan {
    /// The scan root, resolved. What the cache is keyed by, and what
    /// makes two spellings of one tree address one set of entries.
    pub root: PathBuf,
    /// The root exactly as the caller gave it.
    ///
    /// Kept because canonicalizing is right for *addressing* and wrong
    /// for *recording*: a provider that stores "where I scanned" should
    /// store what the user configured, or a deliberate symlink
    /// indirection silently becomes its current target. `rel` is
    /// unaffected either way — a root-relative path is the same
    /// whichever spelling of the root you started from.
    pub root_as_given: PathBuf,
    pub files: Vec<ScannedFile>,
    pub errors: Vec<WalkError>,
    pub stats: ScanStats,
}

/// A file that is at a new path but whose bytes the caller already has.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moved {
    /// Where the caller last saw this content.
    pub was: String,
    pub now: ScannedFile,
}

/// What the caller has not dealt with yet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Changes {
    /// New path, and content the caller has nowhere else.
    pub added: Vec<ScannedFile>,
    /// Known path, different content.
    pub modified: Vec<ScannedFile>,
    /// The caller had this path; it is gone.
    pub removed: Vec<(String, Blake3)>,
    /// Same bytes, new path. Nothing to re-read — a content-keyed
    /// store only has to record the new location.
    pub moved: Vec<Moved>,
    /// Same path, same digest.
    pub unchanged: usize,
}

impl Changes {
    /// Whether anything at all needs doing. The whole question, for a
    /// caller that reprocesses wholesale rather than per file — which
    /// is most of them.
    pub fn any(&self) -> bool {
        !self.added.is_empty()
            || !self.modified.is_empty()
            || !self.removed.is_empty()
            || !self.moved.is_empty()
    }

    /// Files whose **content** the caller must read: the added and the
    /// modified. A move is not here — the bytes are already known.
    pub fn needs_reading(&self) -> impl Iterator<Item = &ScannedFile> {
        self.added.iter().chain(self.modified.iter())
    }
}

impl Scan {
    /// What this source has not dealt with, relative to what it had.
    pub fn changes_since(&self, prev: &Watermark) -> Changes {
        let mut changes = Changes::default();
        let now: BTreeSet<&str> = self.files.iter().map(|f| f.rel.as_str()).collect();

        // Paths the caller knew that are gone. Held first, because a
        // move is one of these paired with an addition.
        let mut vanished: Vec<(&String, &Blake3)> = prev
            .iter()
            .filter(|(rel, _)| !now.contains(rel.as_str()))
            .collect();

        for file in &self.files {
            match prev.get(&file.rel) {
                Some(had) if *had == file.blake3 => changes.unchanged += 1,
                Some(_) => changes.modified.push(file.clone()),
                None => {
                    // A path the caller did not have. If its bytes were
                    // somewhere it *did* have and that place is now
                    // gone, this is a move, not an addition — and a
                    // content-keyed store need not re-read a byte.
                    match vanished.iter().position(|(_, h)| **h == file.blake3) {
                        Some(i) => {
                            let (was, _) = vanished.remove(i);
                            changes.moved.push(Moved {
                                was: was.clone(),
                                now: file.clone(),
                            });
                        }
                        None => changes.added.push(file.clone()),
                    }
                }
            }
        }
        changes.removed = vanished
            .into_iter()
            .map(|(rel, hash)| (rel.clone(), *hash))
            .collect();
        changes
    }

    /// The view to persist, once the caller has dealt with everything.
    pub fn watermark(&self) -> Watermark {
        self.files
            .iter()
            .map(|f| (f.rel.clone(), f.blake3))
            .collect()
    }
}

/// Walk `root`, hashing only what the host cache cannot vouch for.
///
/// `accept` is the caller's file filter — `pdf` wants PDFs, `media`
/// wants media. Filtering is deliberately the caller's, and the cache
/// deliberately keeps whatever any consumer has ever hashed, so a
/// narrow scan never costs a broad one its work.
pub async fn scan<A>(
    cache: &FingerprintCache,
    given: &Path,
    opts: &ScanOptions,
    accept: A,
) -> Result<Scan>
where
    A: Fn(&Path) -> bool,
{
    scan_with(cache, given, opts, accept, |_, _| true).await
}

/// [`scan`], plus a veto consulted **after** the stat and **before**
/// any read.
///
/// Some files must not be opened at all. A macOS file evicted to
/// iCloud is "dataless": it has a size and an mtime, and reading a
/// byte silently pulls the whole thing back over the network. A filter
/// on the path cannot see that — only the stat can — and by the time
/// `scan` would hash it the damage is done. So the caller gets to
/// refuse, knowing what it is refusing.
///
/// A refused file is absent from [`Scan::files`] and leaves the cache
/// untouched, so nothing later mistakes "we declined to look" for "we
/// looked and it was empty".
pub async fn scan_with<A, D>(
    cache: &FingerprintCache,
    given: &Path,
    opts: &ScanOptions,
    accept: A,
    admit: D,
) -> Result<Scan>
where
    A: Fn(&Path) -> bool,
    D: Fn(&Path, &std::fs::Metadata) -> bool,
{
    // Resolved, so the cache's keys line up and two spellings of one
    // tree address one set of entries. A user's configured root is
    // rarely canonical — `~/Docs` may be a symlink, may carry a `..`,
    // may have a trailing slash — and all of those must reach the same
    // cache rows. The unresolved form is kept on the `Scan` for callers
    // that record where they scanned.
    let root = given
        .canonicalize()
        .with_context(|| format!("resolve scan root {}", given.display()))?;

    let cached = cache.load_under(&root).await?;
    let mut stats = ScanStats {
        cache_read: cached.len(),
        ..ScanStats::default()
    };

    let (walked, errors) = fswalk::walk_files(&root, &opts.ignore, accept)
        .with_context(|| format!("walk {}", root.display()))?;

    let mut files = Vec::with_capacity(walked.len());
    let mut fresh_prints = Vec::with_capacity(walked.len());
    for entry in walked {
        if !admit(&entry.path, &entry.meta) {
            continue;
        }
        let fresh = fswalk::fresh_stat(&entry.meta);
        if let Some(max) = opts.max_bytes {
            if fresh.size as u64 > max {
                stats.too_large += 1;
                continue;
            }
        }

        let decision = if opts.force_rehash {
            StampDecision::Rehash
        } else {
            fswalk::decide(cached.cursor(&entry.rel), &fresh)
        };
        // A cursor that matches is only useful with the digest that
        // went with it; without one there is nothing to reuse.
        let reusable = matches!(decision, StampDecision::ReuseHash)
            .then(|| cached.blake3(&entry.rel))
            .flatten();

        let blake3 = match reusable {
            Some(hash) => {
                stats.reused += 1;
                stats.bytes_reused += fresh.size as u64;
                hash
            }
            None => match fswalk::hash_file(&entry.path, fresh.size as u64) {
                Ok(hash) => {
                    stats.hashed += 1;
                    stats.bytes_hashed += fresh.size as u64;
                    hash
                }
                Err(e) => {
                    // Unreadable now; surfaced like any other walk
                    // error rather than failing the whole scan.
                    stats.too_large += 0;
                    tracing::warn!(path = %entry.path.display(), error = %e, "fsscan_hash_failed");
                    continue;
                }
            },
        };

        fresh_prints.push(Fingerprint {
            abs_path: abs_key(&root, &entry.rel),
            kind: EntryKind::File,
            blake3,
            cursor: fswalk::StampCursor {
                mtime_ns: fresh.mtime_ns,
                size: fresh.size,
                stamp_kind: fswalk::stamp_kind_for(&fresh),
                inode: fresh.inode,
                dev: fresh.dev,
            },
        });
        files.push(ScannedFile {
            path: entry.path,
            rel: entry.rel,
            size: fresh.size,
            blake3,
        });
    }

    stats.cache_written = fresh_prints.len() as u64;
    cache.store(&fresh_prints).await?;

    Ok(Scan {
        root,
        root_as_given: given.to_path_buf(),
        files,
        errors,
        stats,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(root: &Path, rel: &str, body: &[u8]) {
        let p = root.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }

    async fn fresh_cache(tmp: &Path) -> FingerprintCache {
        FingerprintCache::open(&tmp.join("fingerprints.sqlite"))
            .await
            .unwrap()
    }

    fn all(_: &Path) -> bool {
        true
    }

    async fn scan_all(cache: &FingerprintCache, root: &Path) -> Scan {
        scan(cache, root, &ScanOptions::default(), all)
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_cold_scan_hashes_everything_and_a_warm_one_hashes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "a.txt", b"aaa");
        write(&root, "sub/b.txt", b"bbbb");
        let cache = fresh_cache(tmp.path()).await;

        let cold = scan_all(&cache, &root).await;
        assert_eq!(cold.files.len(), 2);
        assert_eq!(cold.stats.hashed, 2);
        assert_eq!(cold.stats.reused, 0);
        assert_eq!(cold.stats.bytes_hashed, 7);

        let warm = scan_all(&cache, &root).await;
        assert_eq!(warm.stats.hashed, 0);
        assert_eq!(warm.stats.reused, 2);
        assert_eq!(warm.stats.bytes_reused, 7);
    }

    /// The payoff of a shared cache: a second consumer with a narrower
    /// filter pays stat calls and nothing else for what the first one
    /// already hashed.
    #[tokio::test]
    async fn a_second_consumer_reuses_the_first_s_hashes() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "doc.pdf", b"pdf bytes");
        write(&root, "song.mp3", b"mp3 bytes");
        let cache = fresh_cache(tmp.path()).await;

        // A broad scan hashes both.
        let broad = scan_all(&cache, &root).await;
        assert_eq!(broad.stats.hashed, 2);

        // A narrow one reads nothing.
        let narrow = scan(&cache, &root, &ScanOptions::default(), |p| {
            p.extension().is_some_and(|e| e == "pdf")
        })
        .await
        .unwrap();
        assert_eq!(narrow.files.len(), 1);
        assert_eq!(narrow.stats.hashed, 0);
        assert_eq!(narrow.stats.reused, 1);
    }

    #[tokio::test]
    async fn force_rehash_ignores_the_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "a.txt", b"aaa");
        let cache = fresh_cache(tmp.path()).await;
        scan_all(&cache, &root).await;

        let forced = scan(
            &cache,
            &root,
            &ScanOptions {
                force_rehash: true,
                ..ScanOptions::default()
            },
            all,
        )
        .await
        .unwrap();
        assert_eq!(forced.stats.hashed, 1);
        assert_eq!(forced.stats.reused, 0);
    }

    #[tokio::test]
    async fn oversized_files_are_skipped_not_hashed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "small.bin", b"tiny");
        write(&root, "big.bin", &vec![0u8; 4096]);
        let cache = fresh_cache(tmp.path()).await;

        let s = scan(
            &cache,
            &root,
            &ScanOptions {
                max_bytes: Some(100),
                ..ScanOptions::default()
            },
            all,
        )
        .await
        .unwrap();
        assert_eq!(s.files.len(), 1);
        assert_eq!(s.stats.too_large, 1);
        assert_eq!(s.stats.hashed, 1);
    }

    // ── the question the whole thing exists for ──────────────────────

    #[tokio::test]
    async fn nothing_changed_means_nothing_to_do() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "a.txt", b"aaa");
        let cache = fresh_cache(tmp.path()).await;
        let first = scan_all(&cache, &root).await;
        let mark = first.watermark();

        let second = scan_all(&cache, &root).await;
        let changes = second.changes_since(&mark);
        assert!(!changes.any());
        assert_eq!(changes.unchanged, 1);
    }

    #[tokio::test]
    async fn a_new_file_is_added_and_an_edit_is_modified() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "kept.txt", b"same");
        write(&root, "edited.txt", b"before");
        let cache = fresh_cache(tmp.path()).await;
        let mark = scan_all(&cache, &root).await.watermark();

        write(&root, "edited.txt", b"after!!");
        write(&root, "fresh.txt", b"new");
        let changes = scan_all(&cache, &root).await.changes_since(&mark);

        assert_eq!(
            changes.added.iter().map(|f| &f.rel).collect::<Vec<_>>(),
            vec!["fresh.txt"]
        );
        assert_eq!(
            changes.modified.iter().map(|f| &f.rel).collect::<Vec<_>>(),
            vec!["edited.txt"]
        );
        assert_eq!(changes.unchanged, 1);
        assert!(changes.removed.is_empty());
        // Both of those need their bytes read; nothing else does.
        assert_eq!(changes.needs_reading().count(), 2);
    }

    #[tokio::test]
    async fn a_deleted_file_is_removed() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "stays.txt", b"a");
        write(&root, "goes.txt", b"b");
        let cache = fresh_cache(tmp.path()).await;
        let mark = scan_all(&cache, &root).await.watermark();

        std::fs::remove_file(root.join("goes.txt")).unwrap();
        let changes = scan_all(&cache, &root).await.changes_since(&mark);
        assert_eq!(changes.removed.len(), 1);
        assert_eq!(changes.removed[0].0, "goes.txt");
        assert!(changes.added.is_empty());
    }

    /// A rename must not read a byte: the content is already known, so
    /// a content-keyed store only has to record the new location.
    #[tokio::test]
    async fn a_rename_is_a_move_not_an_add_plus_a_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "old/name.txt", b"the same bytes");
        let cache = fresh_cache(tmp.path()).await;
        let mark = scan_all(&cache, &root).await.watermark();

        std::fs::create_dir_all(root.join("new")).unwrap();
        std::fs::rename(root.join("old/name.txt"), root.join("new/name.txt")).unwrap();
        let changes = scan_all(&cache, &root).await.changes_since(&mark);

        assert_eq!(changes.moved.len(), 1, "{changes:?}");
        assert_eq!(changes.moved[0].was, "old/name.txt");
        assert_eq!(changes.moved[0].now.rel, "new/name.txt");
        assert!(changes.added.is_empty(), "a move is not an addition");
        assert!(changes.removed.is_empty(), "a move is not a deletion");
        assert_eq!(
            changes.needs_reading().count(),
            0,
            "a move needs no content read"
        );
    }

    /// A copy leaves the original in place, so it is a genuine addition
    /// — there is nothing vanished to pair it with.
    #[tokio::test]
    async fn a_copy_is_an_addition_not_a_move() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "original.txt", b"shared bytes");
        let cache = fresh_cache(tmp.path()).await;
        let mark = scan_all(&cache, &root).await.watermark();

        write(&root, "copy.txt", b"shared bytes");
        let changes = scan_all(&cache, &root).await.changes_since(&mark);
        assert_eq!(changes.added.len(), 1);
        assert_eq!(changes.added[0].rel, "copy.txt");
        assert!(changes.moved.is_empty());
        assert_eq!(changes.unchanged, 1);
    }

    /// A caller with no prior state sees everything as new, which is
    /// what a first run should look like.
    #[tokio::test]
    async fn an_empty_watermark_makes_everything_added() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "a.txt", b"a");
        write(&root, "b.txt", b"b");
        let cache = fresh_cache(tmp.path()).await;

        let changes = scan_all(&cache, &root)
            .await
            .changes_since(&Watermark::new());
        assert_eq!(changes.added.len(), 2);
        assert_eq!(changes.unchanged, 0);
        assert!(changes.any());
    }

    /// The watermark is the caller's, not the cache's: a second source
    /// that has never seen this tree still gets a full "added" list
    /// even though the cache is warm and reads nothing.
    #[tokio::test]
    async fn a_warm_cache_does_not_hide_work_from_a_new_consumer() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "a.txt", b"a");
        let cache = fresh_cache(tmp.path()).await;
        let first = scan_all(&cache, &root).await;
        assert_eq!(first.stats.hashed, 1);

        let second = scan_all(&cache, &root).await;
        assert_eq!(second.stats.hashed, 0, "the cache should be warm");
        let changes = second.changes_since(&Watermark::new());
        assert_eq!(
            changes.added.len(),
            1,
            "a consumer with no watermark still has everything to do"
        );
    }
}
