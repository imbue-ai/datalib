//! "What changed on the filesystem since I last looked?"
//!
//! ```text
//!     list(cache, root, opts, accept)   →  Listing          what is there, stat only
//!     listing.changes_since(&cursor, _) →  ListingChanges   what to read
//!     changes.read_if_changed(file)     →  ReadFile         its bytes, and their hash
//!
//!     listing.hashed()  or  scan(…)     →  Scan             every file hashed
//!     scan.changes_since(&cursor)       →  Changes          what you haven't dealt with
//! ```
//!
//! A listing reads no file. It knows a hash only where the host-wide
//! [`crate::fingerprint_cache`] can vouch for one, so on a tree nothing has
//! touched it costs stat calls alone. A source that reads a whole file to use
//! it asks for the bytes and gets the hash with them, so a changed file is read
//! once rather than once to hash and once to parse. A source that needs the
//! hash of a file it never reads whole, or needs every hash before it can decide
//! what to read, hashes first, on every core: `Scan`. README.md § "Answering
//! "did it change?" for a file-backed source" says which is which.
//!
//! One walk joining two halves that live apart for good reason. The cache is
//! expensive to compute, identical for every consumer, and deliberately
//! unversioned because it describes a machine rather than a history. The
//! caller's own [`FileScanCursor`] is what *this* source has ingested, and lives
//! in that source's store.
//!
//! The cache cannot answer alone, and that is not a gap to close: it is
//! shared, so another consumer's scan moves it. "Since I last looked" is only
//! well-posed relative to a particular looker.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Mutex;

use anyhow::{Context, Result};
use rayon::prelude::*;

use crate::fingerprint_cache::{abs_key, EntryKind, Fingerprint, FingerprintCache};
use crate::fswalk::{self, Blake3, StampDecision, WalkError};

/// One file as a listing found it: its stat, and its hash where the cache can
/// vouch for it.
#[derive(Debug, Clone)]
pub struct ListedFile {
    /// Absolute path on disk.
    pub path: PathBuf,
    /// Path relative to the scan root, slash-separated.
    pub rel: String,
    pub size: i64,
    /// `None` is a file this host has not hashed since its stat last moved,
    /// which says nothing about whether its bytes did.
    pub known: Option<Blake3>,
    stat: fswalk::FreshStat,
}

/// A file's whole bytes, and the file as they describe it: its hash and
/// size are the ones of these bytes, whatever the listing saw.
#[derive(Debug)]
pub struct ReadFile {
    pub file: ScannedFile,
    pub bytes: Vec<u8>,
}

/// One file with its hash.
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

/// What [`Scan`] needs of the files it holds.
pub trait AtRel {
    fn rel(&self) -> &str;
}

impl AtRel for ListedFile {
    fn rel(&self) -> &str {
        &self.rel
    }
}

impl AtRel for ScannedFile {
    fn rel(&self) -> &str {
        &self.rel
    }
}

/// What a consumer last saw under a root: every file it finished with, and
/// what that file hashed to at the time.
///
/// A [`Scan`] is *now*, this is *then*, and [`Scan::changes_since`] is the
/// difference. Nothing here needs a new table — `pdf` and `media` already keep
/// `path → blake3` as a location index, and a provider keyed only by content
/// can hand back an empty map.
pub type FileScanCursor = BTreeMap<String, Blake3>;

/// Lowercase hex of a digest — the form a provider's own store keeps.
pub fn hex(b: &Blake3) -> String {
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Parse the hex form back. `None` for anything that is not 32 bytes
/// of hex, which a caller reads as "no usable cursor entry here".
pub fn from_hex(s: &str) -> Option<Blake3> {
    if s.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for (i, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(s.get(i * 2..i * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

#[derive(Debug, Clone, Default)]
pub struct ScanOptions {
    /// Extra gitignore-shaped globs from config, on top of any
    /// `.gitignore` found in the tree.
    pub ignore: Vec<String>,
    /// Skip files larger than this rather than hashing them.
    pub max_bytes: Option<u64>,
    /// How deep to walk. `Some(1)` is the root's own files, and no
    /// folder beneath it is opened.
    pub max_depth: Option<usize>,
    /// Where to say what the walk and the hashing are on and how much is left.
    /// Only ever a message, never a bar: a step has one bar, and the
    /// caller's rows are not bytes (`dag/README.md` § "One bar per step").
    pub progress: datalib_etl::progress::Progress,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanStats {
    /// Files whose digest came from the cache — **no content read**.
    pub reused: usize,
    /// Files actually opened and hashed. Zero on a [`Listing`], which opens
    /// nothing.
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

/// One walk's worth of "what is there now". `F` says how much is known of
/// each file; [`Listing`] is one with only what a stat tells.
#[derive(Debug, Clone)]
pub struct Scan<F = ScannedFile> {
    /// The scan root, resolved. What the cache is keyed by, and what
    /// makes two spellings of one tree address one set of entries.
    pub root: PathBuf,
    /// The root exactly as the caller gave it.
    ///
    /// Canonicalizing is right for *addressing* and wrong for *recording*: a
    /// provider storing "where I scanned" should store what the user
    /// configured, or a deliberate symlink indirection silently becomes its
    /// current target.
    pub root_as_given: PathBuf,
    /// The root as given, resolved: the file itself when the caller gave
    /// one file. What a caller keying rows by path under its configured
    /// input strips from [`ScannedFile::path`], which is resolved too —
    /// stripping the unresolved spelling fails whenever a symlink is on
    /// the way.
    pub given_resolved: PathBuf,
    pub files: Vec<F>,
    /// Root-relative paths the walk found and did not read: refused by the
    /// caller's admit hook, over [`ScanOptions::max_bytes`], or
    /// [`Self::unreadable`]. They are there, so a caller keyed by path
    /// keeps their rows.
    pub present_unread: Vec<String>,
    /// Files the walk found and could not open to hash. Each is a problem
    /// of its own ([`Self::report_problems`]), not a walk error: the walk
    /// saw it, so it says nothing about what else is gone.
    pub unreadable: Vec<UnreadableFile>,
    pub errors: Vec<WalkError>,
    pub stats: ScanStats,
}

/// A file the walk found and could not open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnreadableFile {
    pub rel: String,
    /// Why, without the path, which names this machine.
    pub error: String,
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
    /// Entries the walk could not read. A directory that failed to list
    /// looks exactly like one whose files were all deleted, so while this
    /// is non-zero `removed` is not evidence that anything went.
    pub walk_errors: usize,
}

/// Whether root-relative `rel` is inside root-relative directory `rel_dir`,
/// which may be several segments deep. Case-insensitive, because export
/// trees come from other people's tools.
pub fn is_under(rel: &str, rel_dir: &str) -> bool {
    let prefix = format!("{}/", rel_dir.trim_end_matches('/'));
    rel.len() > prefix.len()
        && rel
            .get(..prefix.len())
            .is_some_and(|p| p.eq_ignore_ascii_case(&prefix))
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

    /// [`Self::needs_reading`], narrowed to one directory of the scanned root
    /// ([`is_under`]): an export root holds several feeds' files side by side.
    pub fn needs_reading_under<'a>(
        &'a self,
        rel_dir: &'a str,
    ) -> impl Iterator<Item = &'a ScannedFile> + 'a {
        self.needs_reading()
            .filter(move |f| is_under(&f.rel, rel_dir))
    }

    /// Files whose **content** the caller must read: the added and the
    /// modified. A move is not here — the bytes are already known.
    pub fn needs_reading(&self) -> impl Iterator<Item = &ScannedFile> {
        self.added.iter().chain(self.modified.iter())
    }

    /// For a store keyed by content, whose files are snapshots that can
    /// overlap: whether a record may have left the input — a file gone or
    /// rewritten, after a clean walk. Only a read of every file says which,
    /// so a caller reads them all. An added or moved file only adds.
    pub fn may_have_dropped_records(&self) -> bool {
        self.walk_errors == 0 && (!self.removed.is_empty() || !self.modified.is_empty())
    }

    /// The removed paths a caller may act on: all of them after a clean
    /// walk, none after one that reported errors. For a store keyed by
    /// content, where a move changes nothing, this is the whole question.
    pub fn gone(&self) -> Vec<&str> {
        if self.walk_errors > 0 {
            return Vec::new();
        }
        self.removed.iter().map(|(rel, _)| rel.as_str()).collect()
    }
}

impl Scan<ScannedFile> {
    /// What this source has not dealt with, relative to what it had.
    pub fn changes_since(&self, prev: &FileScanCursor) -> Changes {
        let mut changes = Changes {
            walk_errors: self.errors.len(),
            ..Changes::default()
        };
        let now: BTreeSet<&str> = self
            .files
            .iter()
            .map(|f| f.rel.as_str())
            .chain(self.present_unread.iter().map(String::as_str))
            .collect();

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

    /// The run problem of a store keyed by content that read every file to
    /// learn what the input still holds, and could not read `unread` of
    /// them: nothing was deleted this run.
    pub fn deletions_held_back(unread: usize) -> datalib_etl::download_problems::RunProblem {
        datalib_etl::download_problems::RunProblem::listing(
            "removed_records",
            format!(
                "files were removed or rewritten, but {unread} of the files could not be \
                 read, so no record was deleted this run"
            ),
        )
    }

    /// The view to persist, once the caller has dealt with everything.
    pub fn cursor(&self) -> FileScanCursor {
        self.files
            .iter()
            .map(|f| (f.rel.clone(), f.blake3))
            .collect()
    }
}

impl<F: AtRel> Scan<F> {
    /// Whether the scan holds any file under root-relative `dir`. For an
    /// export, only a product that is here says what was deleted from it: one
    /// missing entirely was left out of the request, so its records stay and
    /// its cursor is kept.
    pub fn any_under(&self, dir: &str) -> bool {
        self.files.iter().any(|f| is_under(f.rel(), dir))
    }

    /// The scanned file at this root-relative path, if the scan saw
    /// one. The single-file feeds' whole question: "is my file there,
    /// and what is in it?"
    pub fn file(&self, rel: &str) -> Option<&F> {
        self.files.iter().find(|f| f.rel() == rel)
    }

    /// Whether `rel` is a path this scan did not find and may only have
    /// failed to see: it lies under, or is, an entry the walk could not
    /// read. A row about such a path was not tried again this run.
    pub fn could_not_see(&self, rel: &str) -> bool {
        self.file(rel).is_none()
            && self
                .errors
                .iter()
                .any(|e| match e.path.strip_prefix(&self.root) {
                    Ok(dir) => {
                        let dir = dir.to_string_lossy();
                        dir.is_empty() || dir == rel || is_under(rel, &dir)
                    }
                    Err(_) => true,
                })
    }

    /// [`Self::could_not_see`] as a value that outlives the scan, for
    /// [`RunProblems::records_tried_all_but`](datalib_etl::run_problems::RunProblems::records_tried_all_but):
    /// the paths this walk has no verdict on.
    pub fn unseen(&self) -> impl Fn(&str) -> bool + Send + Sync + 'static {
        let blind: Vec<Option<String>> = self
            .errors
            .iter()
            .map(|e| {
                e.path
                    .strip_prefix(&self.root)
                    .ok()
                    .map(|dir| dir.to_string_lossy().into_owned())
            })
            .collect();
        // Only a walk with errors has anything it could not see, so only
        // then is the list of what it did see worth holding.
        let seen: std::collections::HashSet<String> = if blind.is_empty() {
            Default::default()
        } else {
            self.files.iter().map(|f| f.rel().to_string()).collect()
        };
        move |rel: &str| {
            !seen.contains(rel)
                && blind.iter().any(|dir| match dir {
                    Some(dir) => dir.is_empty() || dir == rel || is_under(rel, dir),
                    None => true,
                })
        }
    }

    /// Everything this walk could not do, into the run's problems: the
    /// listing row of [`Self::walk_problems_as`], and a
    /// `record:<name>:<path>` row per file it found and could not open. A
    /// file that opens again loses its row; one under an entry the walk
    /// could not read keeps it.
    pub fn report_problems(&self, found: &datalib_etl::run_problems::RunProblems, name: &str) {
        found.extend(self.walk_problems_as(name));
        self.report_unreadable(found, name);
        found.records_tried_all_but(name, self.unseen());
    }

    /// A `record:<name>:<path>` row per file this scan found and could not
    /// open, for a scan hashing part of a listing already reported.
    pub fn report_unreadable(&self, found: &datalib_etl::run_problems::RunProblems, name: &str) {
        found.records_failed(
            self.unreadable.iter().map(|u| {
                datalib_etl::download_problems::RecordProblem::new(name, &u.rel, &u.error)
            }),
        );
    }

    /// The run problem a walk with errors leaves, for the run's
    /// `RunProblems` (`extend`): the files it reports gone keep their
    /// records until a walk completes. Empty for a clean walk, so a run
    /// that reaches its end clears the last run's row.
    pub fn walk_problems(&self) -> Vec<datalib_etl::download_problems::RunProblem> {
        self.walk_problems_as("files")
    }

    /// [`Self::walk_problems`] under a name of the caller's, for a source
    /// that scans more than one tree: the store keeps one row per name.
    pub fn walk_problems_as(&self, name: &str) -> Vec<datalib_etl::download_problems::RunProblem> {
        let Some(first) = self.errors.first() else {
            return Vec::new();
        };
        vec![datalib_etl::download_problems::RunProblem::listing(
            name,
            // The path first, and short: the sample is cut at 80 characters.
            format!(
                "{}: {} ({} unreadable under {}; nothing was deleted this run)",
                match first.path.strip_prefix(&self.root) {
                    Ok(rel) if rel.as_os_str().is_empty() => Path::new("."),
                    Ok(rel) => rel,
                    Err(_) => &first.path,
                }
                .display(),
                first.error,
                self.errors.len(),
                self.root_as_given.display(),
            ),
        )]
    }
}

/// [`list`], then [`Listing::hashed`]: every file with its hash, for a source
/// that needs them before it can decide what to read.
pub async fn scan<A>(
    cache: &FingerprintCache,
    given: &Path,
    opts: &ScanOptions,
    accept: A,
) -> Result<Scan>
where
    A: Fn(&Path) -> bool + Sync,
{
    list(cache, given, opts, accept).await?.hashed().await
}

/// [`list_with`], then [`Listing::hashed`].
pub async fn scan_with<A, D>(
    cache: &FingerprintCache,
    given: &Path,
    opts: &ScanOptions,
    accept: A,
    admit: D,
) -> Result<Scan>
where
    A: Fn(&Path) -> bool + Sync,
    D: Fn(&fswalk::WalkedFile) -> bool + Sync,
{
    list_with(cache, given, opts, accept, admit)
        .await?
        .hashed()
        .await
}

/// Walk `root` and stat what is there, reading no file.
///
/// `accept` is the caller's file filter. Filtering is deliberately the
/// caller's and the cache keeps whatever any consumer has hashed, so a narrow
/// scan never costs a broad one its work.
pub async fn list<A>(
    cache: &FingerprintCache,
    given: &Path,
    opts: &ScanOptions,
    accept: A,
) -> Result<Listing>
where
    A: Fn(&Path) -> bool + Sync,
{
    list_with(cache, given, opts, accept, |_| true).await
}

/// [`list`], plus a veto consulted **after** the stat and **before** any read.
///
/// Some files must not be opened at all: a macOS file evicted to iCloud is
/// "dataless", so reading one byte silently pulls the whole thing back over the
/// network. Only the stat can see that.
///
/// A refused file is absent from [`Scan::files`] and leaves the cache
/// untouched, so nothing later mistakes "we declined to look" for "we looked
/// and it was empty".
pub async fn list_with<A, D>(
    cache: &FingerprintCache,
    given: &Path,
    opts: &ScanOptions,
    accept: A,
    admit: D,
) -> Result<Listing>
where
    A: Fn(&Path) -> bool + Sync,
    D: Fn(&fswalk::WalkedFile) -> bool + Sync,
{
    // Resolved, so the cache's keys line up and two spellings of one tree
    // address one set of entries — a configured root is rarely canonical. The
    // unresolved form stays on the `Scan` for callers that record where they
    // scanned.
    let resolved = given
        .canonicalize()
        .with_context(|| format!("resolve scan root {}", given.display()))?;

    // A single file is a legitimate root. Walking the parent's own entries
    // and accepting only that one name keeps the rest of the function — and
    // the cache keys — identical for both shapes.
    let (root, only, max_depth) = if resolved.is_file() {
        let name = resolved.file_name().map(|n| n.to_os_string());
        match (resolved.parent().map(Path::to_path_buf), name) {
            (Some(parent), Some(name)) => (parent, Some(name), Some(1)),
            _ => (resolved.clone(), None, opts.max_depth),
        }
    } else {
        (resolved.clone(), None, opts.max_depth)
    };

    let cached = cache.load_under(&root).await?;
    let mut stats = ScanStats {
        cache_read: cached.len(),
        ..ScanStats::default()
    };

    let listing_progress = ListProgress::new(&opts.progress, &root);
    let (walked, errors) = fswalk::walk_files(&root, &opts.ignore, max_depth, |p| {
        let accepted = only
            .as_ref()
            .is_none_or(|name| p.file_name() == Some(name.as_os_str()))
            && accept(p);
        if accepted {
            listing_progress.found_one();
        }
        accepted
    })
    .with_context(|| format!("walk {}", root.display()))?;

    let mut files = Vec::with_capacity(walked.len());
    let mut present_unread = Vec::new();
    for entry in walked {
        if !admit(&entry) {
            present_unread.push(entry.rel);
            continue;
        }
        let stat = entry.stat;
        if let Some(max) = opts.max_bytes {
            if stat.size as u64 > max {
                stats.too_large += 1;
                present_unread.push(entry.rel);
                continue;
            }
        }
        let decision = fswalk::decide(cached.cursor(&entry.rel), &stat);
        // A cursor that matches is only useful with the digest that
        // went with it; without one there is nothing to reuse.
        let known = matches!(decision, StampDecision::ReuseHash)
            .then(|| cached.blake3(&entry.rel))
            .flatten();
        if known.is_some() {
            stats.reused += 1;
            stats.bytes_reused += stat.size as u64;
        }
        files.push(ListedFile {
            path: entry.path,
            rel: entry.rel,
            size: stat.size,
            known,
            stat,
        });
    }

    let unvouched = || files.iter().filter(|f| f.known.is_none());
    if unvouched().next().is_some() {
        tracing::info!(
            "file_changes_scan_listed: {} files, {} the cache cannot vouch for, {}",
            files.len(),
            unvouched().count(),
            human_bytes(unvouched().map(|f| f.size as u64).sum())
        );
    }

    Ok(Listing {
        walked: Scan {
            root,
            root_as_given: given.to_path_buf(),
            given_resolved: resolved,
            files,
            present_unread,
            unreadable: Vec::new(),
            errors,
            stats,
        },
        cache: cache.clone(),
        progress: opts.progress.clone(),
        unwritten: Mutex::new(Vec::new()),
        files_read: AtomicUsize::new(0),
        bytes_read: AtomicU64::new(0),
    })
}

/// A walk's stat of every file, no file read. [`Listing::read`] and
/// [`ListingChanges`] read the ones a source wants; [`Listing::hashed`]
/// hashes them for one that needs the hashes first.
pub struct Listing {
    walked: Scan<ListedFile>,
    cache: FingerprintCache,
    progress: datalib_etl::progress::Progress,
    /// Hashes read and not yet in the cache: one write per file would be a
    /// rollback journal per file.
    unwritten: Mutex<Vec<Fingerprint>>,
    files_read: AtomicUsize,
    bytes_read: AtomicU64,
}

impl std::ops::Deref for Listing {
    type Target = Scan<ListedFile>;
    fn deref(&self) -> &Self::Target {
        &self.walked
    }
}

impl Drop for Listing {
    fn drop(&mut self) {
        let lost = self.unwritten.get_mut().map_or(0, |u| u.len());
        if lost > 0 {
            tracing::warn!(
                files = lost,
                "file_changes_scan_unwritten_hashes: a listing was dropped without finish(); the next \
                 scan reads these files again"
            );
        }
    }
}

impl Listing {
    /// Read one file whole. Its hash goes to the host cache under the stat
    /// the listing took *before* this read, so a write landing during it
    /// moves the stat, and the next scan looks again.
    pub async fn read(&self, f: &ListedFile) -> Result<ReadFile> {
        let loud = (f.size as u64 >= LOUD_BYTES).then(|| {
            let name = file_name(&f.path);
            let size = human_bytes(f.size as u64);
            tracing::info!(path = %f.path.display(), "file_changes_scan_reading_file: {name}, {size}");
            self.progress.set_message(&format!("reading {name}: {size}"));
            (name, std::time::Instant::now())
        });
        let bytes = std::fs::read(&f.path).with_context(|| format!("read {}", f.path.display()))?;
        let blake3 = fswalk::hash_bytes(&bytes);
        if let Some((name, started)) = loud {
            tracing::info!(
                path = %f.path.display(),
                "file_changes_scan_read_file: {name} in {:.1}s",
                started.elapsed().as_secs_f64()
            );
        }
        self.files_read.fetch_add(1, Ordering::Relaxed);
        self.bytes_read
            .fetch_add(bytes.len() as u64, Ordering::Relaxed);
        let batch = {
            let mut unwritten = self.unwritten.lock().expect("no panic holds this lock");
            unwritten.push(self.print(&f.rel, &f.stat, blake3));
            if unwritten.len() >= READS_BETWEEN_WRITES {
                std::mem::take(&mut *unwritten)
            } else {
                Vec::new()
            }
        };
        self.cache.store(&batch).await?;
        Ok(ReadFile {
            file: ScannedFile {
                path: f.path.clone(),
                rel: f.rel.clone(),
                size: bytes.len() as i64,
                blake3,
            },
            bytes,
        })
    }

    /// Write the hashes the reads left. Call when the reading is done: one
    /// left behind costs its file a read next scan.
    pub async fn finish(&self) -> Result<()> {
        let files = self.files_read.swap(0, Ordering::Relaxed);
        let bytes = self.bytes_read.swap(0, Ordering::Relaxed);
        if files > 0 {
            tracing::info!(
                "file_changes_scan_read: {files} files, {}",
                human_bytes(bytes)
            );
        }
        let batch = std::mem::take(&mut *self.unwritten.lock().expect("no panic holds this lock"));
        self.cache.store(&batch).await
    }

    fn print(&self, rel: &str, stat: &fswalk::FreshStat, blake3: Blake3) -> Fingerprint {
        Fingerprint {
            abs_path: abs_key(&self.root, rel),
            kind: EntryKind::File,
            blake3,
            cursor: fswalk::StampCursor {
                mtime_ns: stat.mtime_ns,
                size: stat.size,
                ctime_ns: stat.ctime_ns,
                stamp_kind: fswalk::stamp_kind_for(stat),
                inode: stat.inode,
                dev: stat.dev,
            },
        }
    }

    /// Every file with its hash, reading on every core what the cache cannot
    /// vouch for.
    pub async fn hashed(&self) -> Result<Scan> {
        self.hashed_where(|_| true).await
    }

    /// [`Self::hashed`] for the files `wanted` names; the others are left out
    /// of the result and nothing reads them.
    pub async fn hashed_where(&self, wanted: impl Fn(&ListedFile) -> bool) -> Result<Scan> {
        let files: Vec<&ListedFile> = self.files.iter().filter(|f| wanted(f)).collect();
        let to_hash = || files.iter().filter(|f| f.known.is_none());
        let progress = HashProgress::new(
            &self.progress,
            to_hash().count(),
            to_hash().map(|f| f.size as u64).sum(),
        );

        let mut stats = ScanStats {
            reused: 0,
            ..self.stats.clone()
        };
        stats.bytes_reused = 0;
        let mut out = Vec::with_capacity(files.len());
        let mut present_unread = self.present_unread.clone();
        let mut unreadable = Vec::new();
        // Written back between batches, so a scan that is stopped halfway
        // resumes instead of starting over.
        for batch in batches_of_bytes(&files, FLUSH_BYTES) {
            let hashes: Vec<Result<Blake3>> = batch
                .par_iter()
                .map(|f| match f.known {
                    Some(hash) => Ok(hash),
                    None => progress.file(&f.path, f.size as u64),
                })
                .collect();
            let mut prints = Vec::new();
            for (f, hash) in batch.iter().zip(hashes) {
                let blake3 = match hash {
                    Ok(hash) => hash,
                    Err(e) => {
                        // There, and unreadable now: kept as present, so
                        // nothing reads it as deleted, and a problem of its own.
                        tracing::warn!(path = %f.path.display(), error = %e, "file_changes_scan_hash_failed");
                        unreadable.push(UnreadableFile {
                            rel: f.rel.clone(),
                            error: format!("could not open to hash: {}", e.root_cause()),
                        });
                        present_unread.push(f.rel.clone());
                        continue;
                    }
                };
                if f.known.is_some() {
                    stats.reused += 1;
                    stats.bytes_reused += f.size as u64;
                } else {
                    stats.hashed += 1;
                    stats.bytes_hashed += f.size as u64;
                    prints.push(self.print(&f.rel, &f.stat, blake3));
                }
                out.push(ScannedFile {
                    path: f.path.clone(),
                    rel: f.rel.clone(),
                    size: f.size,
                    blake3,
                });
            }
            stats.cache_written += prints.len() as u64;
            self.cache.store(&prints).await?;
        }
        progress.finish();

        Ok(Scan {
            root: self.root.clone(),
            root_as_given: self.root_as_given.clone(),
            given_resolved: self.given_resolved.clone(),
            files: out,
            present_unread,
            unreadable,
            errors: self.errors.clone(),
            stats,
        })
    }

    /// What this source has to look at, relative to what it had, among the
    /// files `wanted` names. A file `wanted` leaves out is not this source's
    /// to read: naming only its own is what lets it say a vanished file is
    /// gone rather than possibly moved to a file it never read.
    pub fn changes_since<'a>(
        &'a self,
        prev: &'a FileScanCursor,
        wanted: impl Fn(&str) -> bool,
    ) -> ListingChanges<'a> {
        let now: HashSet<&str> = self
            .files
            .iter()
            .map(|f| f.rel.as_str())
            .chain(self.present_unread.iter().map(String::as_str))
            .collect();
        let mut hashes = HashMap::new();
        let mut unchanged = 0;
        let mut to_read = Vec::new();
        for f in self.files.iter().filter(|f| wanted(&f.rel)) {
            match f.known {
                Some(hash) if prev.get(&f.rel) == Some(&hash) => unchanged += 1,
                known => {
                    if let Some(hash) = known {
                        hashes.insert(f.rel.as_str(), hash);
                    }
                    to_read.push(f);
                }
            }
        }
        ListingChanges {
            listing: self,
            prev,
            to_read,
            vanished: prev
                .iter()
                .filter(|(rel, _)| !now.contains(rel.as_str()) && wanted(rel))
                .map(|(rel, hash)| (rel.as_str(), *hash))
                .collect(),
            unchanged: AtomicUsize::new(unchanged),
            walk_errors: self.errors.len(),
            state: Mutex::new(ChangesState {
                hashes,
                moved_from: HashSet::new(),
            }),
        }
    }
}

/// What a source has to look at, from a listing, and what it learns as it
/// reads. Whether a file with a moved stat holds new bytes is only known once
/// its bytes are read, so reading is how this is asked.
pub struct ListingChanges<'a> {
    listing: &'a Listing,
    prev: &'a FileScanCursor,
    to_read: Vec<&'a ListedFile>,
    /// Paths the caller had that are not there. Each is either gone or moved,
    /// and only the hash of a new path says which.
    vanished: Vec<(&'a str, Blake3)>,
    unchanged: AtomicUsize,
    /// Entries the walk could not read. A directory that failed to list
    /// looks exactly like one whose files were all deleted.
    pub walk_errors: usize,
    state: Mutex<ChangesState<'a>>,
}

struct ChangesState<'a> {
    /// The hash of every file in `to_read` known so far.
    hashes: HashMap<&'a str, Blake3>,
    /// Vanished paths [`ListingChanges::read_if_changed`] has taken as moved.
    moved_from: HashSet<&'a str>,
}

impl<'a> ListingChanges<'a> {
    /// The files to look at: those whose bytes may differ from what the
    /// cursor says. Ask for each with [`Self::read_if_changed`] or
    /// [`Self::read_if_changed_by_path`].
    pub fn to_read(&self) -> &[&'a ListedFile] {
        &self.to_read
    }

    /// Files whose bytes are what the cursor says. Grows as reads find a file
    /// that was only touched.
    pub fn unchanged(&self) -> usize {
        self.unchanged.load(Ordering::Relaxed)
    }

    /// For a store keyed by content. `None` when the file holds what the cursor
    /// says, or what it says of a path now vanished: the bytes are already
    /// held, so a move is nothing to do.
    pub async fn read_if_changed(&self, f: &'a ListedFile) -> Result<Option<ReadFile>> {
        if let Some(hash) = f.known {
            if self.take_as_moved(f, hash) {
                return Ok(None);
            }
        }
        let Some(read) = self.read_if_changed_by_path(f).await? else {
            return Ok(None);
        };
        Ok((!self.take_as_moved(f, read.file.blake3)).then_some(read))
    }

    /// For a store keyed by path, where a move is new rows at the new path, so
    /// a moved file is read. `None` when the file holds what the cursor says.
    pub async fn read_if_changed_by_path(&self, f: &'a ListedFile) -> Result<Option<ReadFile>> {
        let read = self.listing.read(f).await?;
        self.state
            .lock()
            .expect("no panic holds this lock")
            .hashes
            .insert(f.rel.as_str(), read.file.blake3);
        if self.prev.get(&f.rel) == Some(&read.file.blake3) {
            self.unchanged.fetch_add(1, Ordering::Relaxed);
            return Ok(None);
        }
        Ok(Some(read))
    }

    fn take_as_moved(&self, f: &ListedFile, hash: Blake3) -> bool {
        if self.prev.contains_key(&f.rel) {
            return false;
        }
        let mut state = self.state.lock().expect("no panic holds this lock");
        let from = self
            .vanished
            .iter()
            .find(|(rel, h)| *h == hash && !state.moved_from.contains(rel));
        match from {
            Some((rel, _)) => {
                state.moved_from.insert(rel);
                true
            }
            None => false,
        }
    }

    /// Each vanished path, and where its bytes went if a file here holds them.
    /// `None` when some file here was never read, since it may be where any of
    /// them went.
    fn vanished_and_where(&self) -> Option<Vec<(&'a str, Option<&'a str>)>> {
        let state = self.state.lock().expect("no panic holds this lock");
        let mut taken = HashSet::new();
        let mut moves: HashMap<&'a str, &'a str> = HashMap::new();
        for f in &self.to_read {
            let hash = state.hashes.get(f.rel.as_str())?;
            if self.prev.contains_key(&f.rel) {
                continue;
            }
            let from = self
                .vanished
                .iter()
                .find(|(rel, h)| h == hash && !taken.contains(rel));
            if let Some((rel, _)) = from {
                taken.insert(*rel);
                moves.insert(rel, f.rel.as_str());
            }
        }
        Some(
            self.vanished
                .iter()
                .map(|(rel, _)| (*rel, moves.get(rel).copied()))
                .collect(),
        )
    }

    /// The removed paths a store keyed by content may act on, once the reading
    /// is done: none after a walk with errors, or while a file here went
    /// unread.
    pub fn gone(&self) -> Vec<&'a str> {
        if self.walk_errors > 0 {
            return Vec::new();
        }
        let vanished = self.vanished_and_where().unwrap_or_default();
        vanished
            .into_iter()
            .filter(|(_, moved_to)| moved_to.is_none())
            .map(|(rel, _)| rel)
            .collect()
    }

    /// The paths whose rows a store keyed by path should delete, once the
    /// reading is done: every path gone, and a moved file's old path once
    /// `read` holds its new one, so content is never left keyed nowhere. None
    /// after a walk with errors, or while a file here went unread.
    pub fn gone_by_path(&self, read: &BTreeSet<&str>) -> Vec<&'a str> {
        if self.walk_errors > 0 {
            return Vec::new();
        }
        let vanished = self.vanished_and_where().unwrap_or_default();
        vanished
            .into_iter()
            .filter(|(_, moved_to)| moved_to.is_none_or(|to| read.contains(to)))
            .map(|(rel, _)| rel)
            .collect()
    }
}

/// `files` cut into runs of about `limit` bytes.
fn batches_of_bytes<'a>(files: &'a [&'a ListedFile], limit: u64) -> Vec<&'a [&'a ListedFile]> {
    let mut batches = Vec::new();
    let (mut start, mut bytes) = (0, 0u64);
    for (i, f) in files.iter().enumerate() {
        if f.known.is_none() {
            bytes += f.size as u64;
        }
        if bytes >= limit {
            batches.push(&files[start..=i]);
            (start, bytes) = (i + 1, 0);
        }
    }
    if start < files.len() {
        batches.push(&files[start..]);
    }
    batches
}

/// Says through `Progress::set_message` what the walk is on, since a slow
/// tree can take a while. A count of the files the caller accepted, as the
/// walk finds them on every core.
struct ListProgress<'a> {
    progress: &'a datalib_etl::progress::Progress,
    name: String,
    found: AtomicUsize,
    last_message: Mutex<std::time::Instant>,
}

impl<'a> ListProgress<'a> {
    fn new(progress: &'a datalib_etl::progress::Progress, root: &Path) -> Self {
        let name = file_name(root);
        progress.set_message(&format!("listing {name}"));
        Self {
            progress,
            name,
            found: AtomicUsize::new(0),
            last_message: Mutex::new(std::time::Instant::now()),
        }
    }

    fn found_one(&self) {
        let found = self.found.fetch_add(1, Ordering::Relaxed) + 1;
        let mut last = self.last_message.lock().expect("no panic holds this lock");
        if last.elapsed() >= MESSAGE_EVERY {
            *last = std::time::Instant::now();
            self.progress
                .set_message(&format!("listing {}: {found} files so far", self.name));
        }
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Hashed bytes between writes of the fingerprint cache.
const FLUSH_BYTES: u64 = 512 * 1024 * 1024;
/// Files read whole between writes of the fingerprint cache.
const READS_BETWEEN_WRITES: usize = 512;
/// A file this big gets a line in the log when its hash starts and ends.
const LOUD_BYTES: u64 = 64 * 1024 * 1024;
const MESSAGE_EVERY: std::time::Duration = std::time::Duration::from_millis(500);

/// Says, through `Progress::set_message` and the log, what the hashing
/// pass is on. Silent when there is nothing to hash.
struct HashProgress<'a> {
    progress: &'a datalib_etl::progress::Progress,
    files_total: usize,
    bytes_total: u64,
    files_done: AtomicUsize,
    bytes_done: AtomicU64,
    last_message: Mutex<std::time::Instant>,
}

impl<'a> HashProgress<'a> {
    fn new(
        progress: &'a datalib_etl::progress::Progress,
        files_total: usize,
        bytes_total: u64,
    ) -> Self {
        if files_total > 0 {
            tracing::info!(
                files = files_total,
                bytes = bytes_total,
                "file_changes_scan_hashing: {files_total} files, {} to read",
                human_bytes(bytes_total)
            );
        }
        Self {
            progress,
            files_total,
            bytes_total,
            files_done: AtomicUsize::new(0),
            bytes_done: AtomicU64::new(0),
            last_message: Mutex::new(std::time::Instant::now() - MESSAGE_EVERY),
        }
    }

    fn file(&self, path: &Path, size: u64) -> Result<Blake3> {
        let name = file_name(path);
        let started = std::time::Instant::now();
        if size >= LOUD_BYTES {
            tracing::info!(path = %path.display(), "file_changes_scan_hashing_file: {name}, {}", human_bytes(size));
        }
        let mut counted = 0;
        let hash = fswalk::hash_file_reporting(path, size, |in_file| {
            let done = self
                .bytes_done
                .fetch_add(in_file - counted, Ordering::Relaxed);
            let done = done + (in_file - counted);
            counted = in_file;
            self.say(&name, done);
        })?;
        self.files_done.fetch_add(1, Ordering::Relaxed);
        if size >= LOUD_BYTES {
            tracing::info!(
                path = %path.display(),
                "file_changes_scan_hashed_file: {name} in {:.1}s",
                started.elapsed().as_secs_f64()
            );
        }
        Ok(hash)
    }

    fn say(&self, name: &str, bytes_done: u64) {
        let mut last = self.last_message.lock().expect("no panic holds this lock");
        if last.elapsed() >= MESSAGE_EVERY {
            *last = std::time::Instant::now();
            self.progress.set_message(&format!(
                "hashing {name} (file {} of {}): {} of {}",
                self.files_done.load(Ordering::Relaxed) + 1,
                self.files_total,
                human_bytes(bytes_done),
                human_bytes(self.bytes_total)
            ));
        }
    }

    fn finish(&self) {
        if self.files_total > 0 {
            tracing::info!(
                "file_changes_scan_hashed: {} files, {}",
                self.files_done.load(Ordering::Relaxed),
                human_bytes(self.bytes_done.load(Ordering::Relaxed))
            );
        }
    }
}

fn human_bytes(n: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1000.0 && u < UNITS.len() - 1 {
        v /= 1000.0;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", UNITS[u])
    }
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

    /// A file past the parallel threshold must hash to exactly what
    /// `blake3::hash` says, and report a rising byte count on the way.
    #[test]
    fn a_big_file_hashes_in_chunks_to_the_same_digest() {
        let tmp = tempfile::tempdir().unwrap();
        let body: Vec<u8> = (0..20_000_000u32).map(|i| (i % 251) as u8).collect();
        let path = tmp.path().join("big.bin");
        std::fs::write(&path, &body).unwrap();

        let mut seen = Vec::new();
        let got = fswalk::hash_file_reporting(&path, body.len() as u64, |n| seen.push(n)).unwrap();

        assert_eq!(got, *blake3::hash(&body).as_bytes());
        assert!(seen.len() > 1 && seen.windows(2).all(|w| w[0] < w[1]));
        assert_eq!(seen.last(), Some(&(body.len() as u64)));
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

    /// A file as the root is that file: not a same-named file in a
    /// folder beside it, and no walk of the folders beside it at all. A
    /// catalog next to its `Previews.lrdata`, or a download in
    /// `~/Downloads`, used to cost a walk of everything under the parent.
    #[tokio::test]
    async fn a_file_root_is_that_file_and_nothing_under_its_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "Catalog.lrcat", b"the catalog");
        write(&root, "Catalog Previews.lrdata/Catalog.lrcat", b"not it");
        let cache = fresh_cache(tmp.path()).await;

        let scan = scan_all(&cache, &root.join("Catalog.lrcat")).await;
        let rels: Vec<&str> = scan.files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, ["Catalog.lrcat"]);
    }

    /// `max_depth: Some(1)` is the root's own files and none beneath it.
    #[tokio::test]
    async fn max_depth_one_stays_in_the_root_folder() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "top.txt", b"top");
        write(&root, "sub/deep.txt", b"deep");
        let cache = fresh_cache(tmp.path()).await;
        let opts = ScanOptions {
            max_depth: Some(1),
            ..ScanOptions::default()
        };
        let scan = scan(&cache, &root, &opts, all).await.unwrap();
        let rels: Vec<&str> = scan.files.iter().map(|f| f.rel.as_str()).collect();
        assert_eq!(rels, ["top.txt"]);
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
        assert_eq!(s.present_unread, vec!["big.bin".to_string()]);
    }

    // ── the question the whole thing exists for ──────────────────────

    #[tokio::test]
    async fn nothing_changed_means_nothing_to_do() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "a.txt", b"aaa");
        let cache = fresh_cache(tmp.path()).await;
        let first = scan_all(&cache, &root).await;
        let mark = first.cursor();

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
        let mark = scan_all(&cache, &root).await.cursor();

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
        let mark = scan_all(&cache, &root).await.cursor();

        std::fs::remove_file(root.join("goes.txt")).unwrap();
        let changes = scan_all(&cache, &root).await.changes_since(&mark);
        assert_eq!(changes.removed.len(), 1);
        assert_eq!(changes.removed[0].0, "goes.txt");
        assert!(changes.added.is_empty());
    }

    /// A walk that hit an error cannot tell a deleted file from one it
    /// failed to see, so nothing it reports gone is deleted.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_walk_error_deletes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "stays.vcf", b"a");
        write(&root, "goes.vcf", b"b");
        let cache = fresh_cache(tmp.path()).await;
        let mark = scan_all(&cache, &root).await.cursor();

        std::fs::remove_file(root.join("goes.vcf")).unwrap();
        std::os::unix::fs::symlink(root.join("nowhere"), root.join("dangling.vcf")).unwrap();
        let changes = scan_all(&cache, &root).await.changes_since(&mark);

        assert_eq!(changes.walk_errors, 1, "{changes:?}");
        assert_eq!(changes.removed.len(), 1, "the removal is still reported");
        assert!(changes.gone().is_empty());
    }

    /// A file that will not open is there: it was a walk error, so one
    /// unreadable file held back every deletion under the root. It is
    /// present and unread now, with a problem of its own, and the file
    /// beside it that really went is gone.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_file_that_will_not_open_is_present_not_a_walk_error() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "locked.vcf", b"a");
        write(&root, "goes.vcf", b"b");
        let cache = fresh_cache(tmp.path()).await;
        let mark = scan_all(&cache, &root).await.cursor();

        std::fs::remove_file(root.join("goes.vcf")).unwrap();
        let locked = root.join("locked.vcf");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&locked).is_ok() {
            // Root reads through any mode; CI's container runs as root.
            return;
        }
        let scan = scan_all(&cache, &root).await;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert!(scan.errors.is_empty(), "{:?}", scan.errors);
        assert_eq!(scan.present_unread, ["locked.vcf"]);
        assert_eq!(scan.unreadable.len(), 1);
        assert_eq!(scan.unreadable[0].rel, "locked.vcf");
        assert!(
            !scan.unreadable[0].error.contains(&*root.to_string_lossy()),
            "the problem names no machine's path: {}",
            scan.unreadable[0].error
        );
        let changes = scan.changes_since(&mark);
        assert_eq!(changes.gone(), ["goes.vcf"], "{changes:?}");
    }

    /// A rename must not read a byte: the content is already known, so
    /// a content-keyed store only has to record the new location.
    #[tokio::test]
    async fn a_rename_is_a_move_not_an_add_plus_a_delete() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "old/name.txt", b"the same bytes");
        let cache = fresh_cache(tmp.path()).await;
        let mark = scan_all(&cache, &root).await.cursor();

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
        let mark = scan_all(&cache, &root).await.cursor();

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
    async fn an_empty_cursor_makes_everything_added() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "a.txt", b"a");
        write(&root, "b.txt", b"b");
        let cache = fresh_cache(tmp.path()).await;

        let changes = scan_all(&cache, &root)
            .await
            .changes_since(&FileScanCursor::new());
        assert_eq!(changes.added.len(), 2);
        assert_eq!(changes.unchanged, 0);
        assert!(changes.any());
    }

    /// The cursor is the caller's, not the cache's: a second source
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
        let changes = second.changes_since(&FileScanCursor::new());
        assert_eq!(
            changes.added.len(),
            1,
            "a consumer with no cursor still has everything to do"
        );
    }

    // ── a listing reads nothing, and reading is how it learns ─────────

    async fn list_all(cache: &FingerprintCache, root: &Path) -> Listing {
        list(cache, root, &ScanOptions::default(), all)
            .await
            .unwrap()
    }

    /// A file that will not open lists like any other: the walk asks for its
    /// stat and nothing else. It only fails when a source reads it.
    #[cfg(unix)]
    #[tokio::test]
    async fn a_listing_opens_no_file() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "locked.txt", b"a");
        let locked = root.join("locked.txt");
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).unwrap();
        if std::fs::read(&locked).is_ok() {
            // Root reads through any mode; CI's container runs as root.
            return;
        }
        let cache = fresh_cache(tmp.path()).await;
        let listing = list_all(&cache, &root).await;
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o644)).unwrap();

        assert_eq!(listing.files.len(), 1);
        assert_eq!(listing.files[0].known, None);
        assert_eq!(listing.stats.hashed, 0);
    }

    /// The hash a read found is the one a later scan reuses, once `finish`
    /// has written it; a reader's read saves the scan a read.
    #[tokio::test]
    async fn a_read_vouches_for_the_file_next_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "a.txt", b"aaa");
        let cache = fresh_cache(tmp.path()).await;

        let first = list_all(&cache, &root).await;
        let read = first.read(&first.files[0]).await.unwrap();
        assert_eq!(read.bytes, b"aaa");
        assert_eq!(read.file.blake3, *blake3::hash(b"aaa").as_bytes());
        assert_eq!(read.file.size, 3);
        first.finish().await.unwrap();

        let second = list_all(&cache, &root).await;
        assert_eq!(second.files[0].known, Some(read.file.blake3));
        assert_eq!(second.stats.reused, 1);
        let scan = second.hashed().await.unwrap();
        assert_eq!(scan.stats.hashed, 0, "the read already hashed it");
    }

    /// The hash goes in the cache under the stat the listing took before the
    /// read, so bytes written after that stat are looked at again: the stat
    /// has moved by the next scan, whatever this read saw.
    #[tokio::test]
    async fn a_write_after_the_listing_is_seen_next_scan() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "a.txt", b"before");
        let cache = fresh_cache(tmp.path()).await;

        let first = list_all(&cache, &root).await;
        write(&root, "a.txt", b"written after the listing");
        let read = first.read(&first.files[0]).await.unwrap();
        assert_eq!(read.bytes, b"written after the listing");
        first.finish().await.unwrap();

        let second = list_all(&cache, &root).await;
        assert_eq!(
            second.files[0].known, None,
            "the stat moved after the listing"
        );
    }

    /// A file only touched costs a read and nothing more: its bytes are what
    /// the cursor says, so the source neither parses nor stamps it.
    #[tokio::test]
    async fn a_touched_file_is_read_once_and_is_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "a.txt", b"same");
        let cache = fresh_cache(tmp.path()).await;
        let mark = scan_all(&cache, &root).await.cursor();

        let file = std::fs::File::options()
            .write(true)
            .open(root.join("a.txt"))
            .unwrap();
        file.set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs(1))
            .unwrap();

        let listing = list_all(&cache, &root).await;
        let changes = listing.changes_since(&mark, |_| true);
        assert_eq!(changes.unchanged(), 0);
        let f = changes.to_read()[0];
        assert!(changes.read_if_changed(f).await.unwrap().is_none());
        assert_eq!(changes.unchanged(), 1);
        listing.finish().await.unwrap();

        let listing = list_all(&cache, &root).await;
        let changes = listing.changes_since(&mark, |_| true);
        assert_eq!(changes.to_read().len(), 0, "the read vouched for it");
        assert_eq!(changes.unchanged(), 1);
    }

    /// A store keyed by content has the bytes of a moved file already; a
    /// store keyed by path reads it, and loses the old path's rows only once
    /// the new one is read.
    #[tokio::test]
    async fn a_listing_tells_a_move_once_the_new_path_is_read() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "old.vcf", b"moved bytes");
        write(&root, "goes.vcf", b"b");
        let cache = fresh_cache(tmp.path()).await;
        let mark = scan_all(&cache, &root).await.cursor();
        std::fs::rename(root.join("old.vcf"), root.join("new.vcf")).unwrap();
        std::fs::remove_file(root.join("goes.vcf")).unwrap();

        let listing = list_all(&cache, &root).await;

        let by_content = listing.changes_since(&mark, |_| true);
        assert!(by_content.gone().is_empty(), "the new path is not read yet");
        let f = by_content.to_read()[0];
        assert!(by_content.read_if_changed(f).await.unwrap().is_none());
        assert_eq!(by_content.gone(), ["goes.vcf"]);

        let by_path = listing.changes_since(&mark, |_| true);
        let f = by_path.to_read()[0];
        let read = by_path.read_if_changed_by_path(f).await.unwrap().unwrap();
        assert_eq!(read.file.rel, "new.vcf");
        assert_eq!(by_path.gone_by_path(&BTreeSet::new()), ["goes.vcf"]);
        assert_eq!(
            sorted(by_path.gone_by_path(&BTreeSet::from(["new.vcf"]))),
            ["goes.vcf", "old.vcf"],
        );
        listing.finish().await.unwrap();
    }

    fn sorted(mut v: Vec<&str>) -> Vec<&str> {
        v.sort_unstable();
        v
    }

    /// A file not read could be where a vanished one went, so nothing is
    /// called gone yet. A stopped run or a file that will not open deletes
    /// nothing.
    #[tokio::test]
    async fn nothing_is_gone_while_a_file_went_unread() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "goes.txt", b"b");
        let cache = fresh_cache(tmp.path()).await;
        let mark = scan_all(&cache, &root).await.cursor();
        std::fs::remove_file(root.join("goes.txt")).unwrap();
        write(&root, "new.txt", b"new");

        let listing = list_all(&cache, &root).await;
        let changes = listing.changes_since(&mark, |_| true);
        assert!(changes.gone().is_empty());
        assert!(changes.gone_by_path(&BTreeSet::new()).is_empty());

        let f = changes.to_read()[0];
        assert!(changes.read_if_changed(f).await.unwrap().is_some());
        assert_eq!(changes.gone(), ["goes.txt"]);
        listing.finish().await.unwrap();
    }

    /// A feed of a shared export reads only its own files. Another feed's
    /// file is not one it failed to read, so it holds back no deletion.
    #[tokio::test]
    async fn wanted_leaves_other_feeds_files_out() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "mine/goes.txt", b"b");
        let cache = fresh_cache(tmp.path()).await;
        let mark = scan_all(&cache, &root).await.cursor();
        std::fs::remove_file(root.join("mine/goes.txt")).unwrap();
        write(&root, "theirs/new.txt", b"new");

        let listing = list_all(&cache, &root).await;
        let changes = listing.changes_since(&mark, |rel| is_under(rel, "mine"));
        assert!(changes.to_read().is_empty());
        assert_eq!(changes.gone(), ["mine/goes.txt"]);
    }

    /// A warm scan learns nothing, so it writes nothing: rewriting every row
    /// would be a write per file to say that no file changed.
    #[tokio::test]
    async fn a_warm_scan_writes_no_cache_row() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "a.txt", b"a");
        write(&root, "b.txt", b"b");
        let cache = fresh_cache(tmp.path()).await;
        assert_eq!(scan_all(&cache, &root).await.stats.cache_written, 2);
        assert_eq!(scan_all(&cache, &root).await.stats.cache_written, 0);
        write(&root, "b.txt", b"bb");
        assert_eq!(scan_all(&cache, &root).await.stats.cache_written, 1);
    }

    /// Hashing runs on every core and the files come back in the walk's
    /// order, each with its own hash, whatever order the hashes finished in.
    #[tokio::test]
    async fn parallel_hashing_keeps_each_hash_with_its_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        for i in 0..200 {
            write(
                &root,
                &format!("d{}/f{i:03}", i % 7),
                format!("body {i}").as_bytes(),
            );
        }
        let cache = fresh_cache(tmp.path()).await;
        let scan = scan_all(&cache, &root).await;
        assert_eq!(scan.files.len(), 200);
        let rels: Vec<&str> = scan.files.iter().map(|f| f.rel.as_str()).collect();
        let mut in_order = rels.clone();
        in_order.sort_unstable();
        assert_eq!(rels, in_order);
        for f in &scan.files {
            let i: usize = f.rel.rsplit("/f").next().unwrap().parse().unwrap();
            assert_eq!(
                f.blake3,
                *blake3::hash(format!("body {i}").as_bytes()).as_bytes()
            );
        }
    }

    /// Asking for part of a listing hashes that part and opens nothing else.
    #[tokio::test]
    async fn hashed_where_reads_only_what_it_names() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "voice/a.txt", b"a");
        write(&root, "maps/b.txt", b"b");
        let cache = fresh_cache(tmp.path()).await;
        let listing = list_all(&cache, &root).await;
        let voice = listing
            .hashed_where(|f| is_under(&f.rel, "voice"))
            .await
            .unwrap();
        assert_eq!(voice.files.len(), 1);
        assert_eq!(voice.stats.hashed, 1);
        let next = list_all(&cache, &root).await;
        assert!(
            next.file("maps/b.txt").unwrap().known.is_none(),
            "never hashed"
        );
        assert!(next.file("voice/a.txt").unwrap().known.is_some());
    }

    // ── what a scan says while it runs ────────────────────────────────

    #[derive(Default)]
    struct Said(Mutex<Vec<String>>);

    impl datalib_etl::progress::ProgressSink for Said {
        fn set_message(&self, msg: &str) {
            self.0.lock().unwrap().push(msg.to_string());
        }
    }

    fn saying() -> (std::sync::Arc<Said>, ScanOptions) {
        let said = std::sync::Arc::new(Said::default());
        let opts = ScanOptions {
            progress: datalib_etl::progress::Progress::new(said.clone()),
            ..ScanOptions::default()
        };
        (said, opts)
    }

    /// A step's row said nothing while a cold scan hashed a tree, however
    /// long that took, for every source that left this out of its options.
    #[tokio::test]
    async fn a_scan_says_what_it_is_walking_and_hashing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "a.txt", b"aaa");
        let cache = fresh_cache(tmp.path()).await;
        let (said, opts) = saying();

        scan(&cache, &root, &opts, all).await.unwrap();

        let said = said.0.lock().unwrap();
        assert_eq!(said[0], "listing tree", "{said:?}");
        assert!(
            said.iter()
                .any(|m| m.starts_with("hashing a.txt (file 1 of 1)")),
            "{said:?}"
        );
    }

    /// Nothing to hash, nothing said about hashing.
    #[tokio::test]
    async fn a_warm_scan_says_nothing_about_hashing() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        write(&root, "a.txt", b"aaa");
        let cache = fresh_cache(tmp.path()).await;
        scan_all(&cache, &root).await;
        let (said, opts) = saying();

        scan(&cache, &root, &opts, all).await.unwrap();

        let said = said.0.lock().unwrap();
        assert!(!said.iter().any(|m| m.contains("hashing")), "{said:?}");
    }

    /// Reading a huge file through a listing is one long call with nothing to
    /// report partway, so it says so before it starts.
    #[tokio::test]
    async fn a_large_read_says_what_it_is_on() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("tree");
        std::fs::create_dir_all(&root).unwrap();
        let big = std::fs::File::create(root.join("big.bin")).unwrap();
        big.set_len(LOUD_BYTES).unwrap();
        write(&root, "small.txt", b"s");
        let cache = fresh_cache(tmp.path()).await;
        let (said, opts) = saying();
        let listing = list(&cache, &root, &opts, all).await.unwrap();

        listing
            .read(listing.file("small.txt").unwrap())
            .await
            .unwrap();
        assert!(!said
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|m| m.starts_with("reading")));

        listing
            .read(listing.file("big.bin").unwrap())
            .await
            .unwrap();
        listing.finish().await.unwrap();
        let said = said.0.lock().unwrap();
        assert_eq!(said.last().unwrap(), "reading big.bin: 67.1 MB", "{said:?}");
    }

    /// Files hash on every core and report as they go: the counts have to come
    /// out true whatever order the threads ran in, chunked files included.
    #[test]
    fn hash_progress_counts_every_file_and_byte_across_threads() {
        let tmp = tempfile::tempdir().unwrap();
        let mut files = Vec::new();
        for i in 0..200u64 {
            let path = tmp.path().join(format!("f{i}"));
            std::fs::write(&path, vec![0u8; i as usize]).unwrap();
            files.push((path, i));
        }
        // Past the size that reads in chunks and reports each one.
        let chunked = tmp.path().join("chunked");
        std::fs::File::create(&chunked)
            .unwrap()
            .set_len(40_000_000)
            .unwrap();
        files.push((chunked, 40_000_000));
        let total: u64 = files.iter().map(|(_, size)| size).sum();
        let (said, opts) = saying();
        let progress = HashProgress::new(&opts.progress, files.len(), total);

        files.par_iter().for_each(|(path, size)| {
            progress.file(path, *size).unwrap();
        });

        assert_eq!(progress.files_done.load(Ordering::Relaxed), files.len());
        assert_eq!(progress.bytes_done.load(Ordering::Relaxed), total);
        assert!(!said.0.lock().unwrap().is_empty());
    }
}
