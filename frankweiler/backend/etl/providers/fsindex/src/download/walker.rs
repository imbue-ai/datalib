//! Bottom-up tree walker that produces one (FileRow, FileStatsRow)
//! pair per visible entry under a root.
//!
//! See [`EXTRACT.md`](../../EXTRACT.md) §"Why two entity tables" and
//! §"The fast-rescan trick" for the row split and reuse-vs-rehash
//! decision. See [`super::schema_raw`] §"Directory tree-hash
//! canonicalization" for the dir hash encoding.
//!
//! CONCERN(tree-hash-spec-one-way): the canonical encoding is a
//! one-way commitment to a byte format. A future change to the
//! encoding can only be rolled out by bumping `scan_meta.scanner_version`
//! so existing dir hashes are explicitly considered stale.
//!
//! We drive a manual depth-first recursion (see [`Dfs`]) rather than
//! using a walk crate, so an unchanged directory (matching cached
//! mtime) can enumerate its children from the in-memory rescan cache
//! and skip the `readdir` syscall entirely — Unison's
//! `unchangedChildren` fast path. CONCERN(perf-unmeasured): performance
//! against the design-target tens-of-millions scale is asserted, not
//! measured.
//!
//! CONCERN(long-tail-fs): non-UTF-8 names, sparse files, files that
//! disappear between readdir and stat, case-insensitive collisions,
//! mtimes in the future — handled coarsely (skip + warn, or
//! propagate-to-bookkeeping) but not exhaustively tested.
//!
//! CONCERN(utf8-paths): the schema requires `files.id TEXT`, which
//! means valid UTF-8. Non-UTF-8 entry names are skipped with a
//! `warn!` and recorded as walker errors. See [`Walker::collect`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;

use anyhow::{Context, Result};
use tracing::warn;

use super::hash::{hash_file, hash_symlink_target, hash_tree, Blake3, TreeChild};
use super::metrics::WalkerCounters;
use super::options::{self, EffectiveOptions, FsindexYaml, OptionsCascade, BREADCRUMB_FILENAME};
use super::schema_raw::{FileKind, FileRow, FileStatsRow, StampKind};
use super::stamp::{self, FreshStat, StampDecision};

/// Soft upper bound on the size of one streamed batch. The walker
/// flushes the batch via the callback when it reaches this many rows.
///
/// This is the memory-vs-amplification knob. Each batch is one sqlite
/// transaction: larger batches mean fewer transactions, which means
/// less write-amplification (every COMMIT lays down fresh prolly chunk
/// novelty that only `dolt_gc` reclaims). But a batch is also buffered
/// in memory — both as `ScanResult`s in our process and as the open
/// transaction's working-set delta inside doltlite — so it can't grow
/// unbounded (a single all-in-one transaction OOMs at multi-million-row
/// scale). 100k rows is ~35 MB of `ScanResult` and keeps the
/// transaction count to ~50 even on a 5M-file tree.
pub const BATCH_SIZE: usize = 100_000;

/// Output row pair from the walk. The walker emits these in
/// post-order (children before their containing dir) so directory
/// hashes can be folded up.
pub struct ScanResult {
    pub file_row: FileRow,
    pub stat_row: FileStatsRow,
}

/// One unreadable entry. Surfaced to the caller so it can land in
/// `<table>_bookkeeping.last_error` per the framework's universal
/// pattern.
pub struct WalkerError {
    pub id: String,
    pub message: String,
}

pub struct WalkerSummary {
    pub rehashed: usize,
    pub reused: usize,
}

pub struct Walker<'a> {
    root: &'a Path,
    prev_stats: &'a HashMap<String, FileStatsRow>,
    prev_file_blake3s: &'a HashMap<String, Blake3>,
    prev_children: &'a HashMap<String, Vec<String>>,
    default_stamp_kind: StampKind,
}

impl<'a> Walker<'a> {
    pub fn new(
        root: &'a Path,
        prev_stats: &'a HashMap<String, FileStatsRow>,
        prev_file_blake3s: &'a HashMap<String, Blake3>,
        prev_children: &'a HashMap<String, Vec<String>>,
        default_stamp_kind: StampKind,
    ) -> Self {
        Self {
            root,
            prev_stats,
            prev_file_blake3s,
            prev_children,
            default_stamp_kind,
        }
    }

    /// Convenience: collect every scan-result into a `Vec<ScanResult>`
    /// in memory. Suitable for tests and the stamping orchestrator
    /// path, NOT for production large-tree scans (memory grows with
    /// row count). Production calls `collect_streaming` directly.
    pub fn collect(&self) -> Result<(Vec<ScanResult>, Vec<WalkerError>, WalkerSummary)> {
        let mut out: Vec<ScanResult> = Vec::new();
        let counters = WalkerCounters::default();
        let (errors, summary) = self.collect_streaming(&counters, |batch| {
            out.extend(batch);
            Ok(())
        })?;
        Ok((out, errors, summary))
    }

    /// Streaming walk. Emits batches of at most [`BATCH_SIZE`] rows
    /// via the `emit_batch` callback so memory stays O(batch_size +
    /// tree_depth) rather than O(total_entries). The callback's
    /// `Err` short-circuits the walk.
    ///
    /// The walker never writes the filesystem. Stamping (which does
    /// write) is the orchestrator's job and happens either before
    /// the walk (preferred, so the walker sees the new breadcrumbs)
    /// or after a separate in-memory `collect()` call (legacy
    /// path).
    pub fn collect_streaming<F>(
        &self,
        counters: &WalkerCounters,
        emit_batch: F,
    ) -> Result<(Vec<WalkerError>, WalkerSummary)>
    where
        F: FnMut(Vec<ScanResult>) -> Result<()>,
    {
        let mut dfs = Dfs {
            root: self.root,
            prev_stats: self.prev_stats,
            prev_blake3s: self.prev_file_blake3s,
            prev_children: self.prev_children,
            default_stamp_kind: self.default_stamp_kind,
            counters,
            config_cache: HashMap::new(),
            cascade_cache: HashMap::new(),
            buf: Vec::with_capacity(BATCH_SIZE),
            errors: Vec::new(),
            summary: WalkerSummary {
                rehashed: 0,
                reused: 0,
            },
            emit: emit_batch,
        };
        dfs.run()?;
        Ok((dfs.errors, dfs.summary))
    }
}

/// Manual depth-first walk driver. We drive the recursion ourselves
/// (rather than the `walkdir` crate) so we can take Unison's
/// `unchangedChildren` fast path: when a directory's own mtime matches
/// the cache, the set of names it contains cannot have changed
/// (adds/removes/renames bump a directory's mtime), so we enumerate its
/// children from the in-memory cache and skip the `readdir` syscall.
/// Every child is still `lstat`ed — an in-place content edit does NOT
/// touch the parent directory's mtime, so that is the only way to catch
/// it — but on a large unchanged tree we save one `readdir` per
/// directory, and (since the cache is fully in memory) touch the disk
/// only for the stats.
///
/// Known gap: a child that was *ignored* on the previous scan is not in
/// the cache, so if a cascade `ignore` rule is *loosened* by editing an
/// existing `.fsindex.yaml` (which does not change the affected
/// directories' mtimes), the newly-unignored entries are not picked up
/// until that directory's mtime next changes or a `--reset-and-redownload`.
/// Newly-*ignored* entries are handled correctly (we re-test the current
/// ignore set against every enumerated child).
struct Dfs<'a, F: FnMut(Vec<ScanResult>) -> Result<()>> {
    root: &'a Path,
    prev_stats: &'a HashMap<String, FileStatsRow>,
    prev_blake3s: &'a HashMap<String, Blake3>,
    prev_children: &'a HashMap<String, Vec<String>>,
    default_stamp_kind: StampKind,
    counters: &'a WalkerCounters,
    config_cache: HashMap<PathBuf, Option<FsindexYaml>>,
    cascade_cache: HashMap<PathBuf, OptionsCascade>,
    buf: Vec<ScanResult>,
    errors: Vec<WalkerError>,
    summary: WalkerSummary,
    emit: F,
}

impl<'a, F: FnMut(Vec<ScanResult>) -> Result<()>> Dfs<'a, F> {
    fn run(&mut self) -> Result<()> {
        let meta = std::fs::metadata(self.root)
            .with_context(|| format!("stat scan root {}", self.root.display()))?;
        if !meta.is_dir() {
            anyhow::bail!("scan root {} is not a directory", self.root.display());
        }
        let root_path = self.root.to_path_buf();
        self.walk_dir(&root_path, "", &meta)?;
        if !self.buf.is_empty() {
            self.counters
                .batches_emitted
                .fetch_add(1, Ordering::Relaxed);
            let drained = std::mem::take(&mut self.buf);
            (self.emit)(drained)?;
        }
        Ok(())
    }

    fn push_row(&mut self, file_row: FileRow, stat_row: FileStatsRow) -> Result<()> {
        self.buf.push(ScanResult { file_row, stat_row });
        self.counters.rows_emitted.fetch_add(1, Ordering::Relaxed);
        if self.buf.len() >= BATCH_SIZE {
            self.counters
                .batches_emitted
                .fetch_add(1, Ordering::Relaxed);
            let drained = std::mem::replace(&mut self.buf, Vec::with_capacity(BATCH_SIZE));
            (self.emit)(drained)?;
        }
        Ok(())
    }

    /// Process one directory: emit rows for all of its descendants and
    /// then for the directory itself (post-order, so child hashes are
    /// known before the parent's tree-hash is folded). Returns the
    /// directory's tree-hash and rolled-up content size.
    fn walk_dir(
        &mut self,
        dir_path: &Path,
        dir_rel: &str,
        dir_meta: &std::fs::Metadata,
    ) -> Result<(Blake3, i64)> {
        self.counters.dirs_visited.fetch_add(1, Ordering::Relaxed);
        let dir_fresh = fresh_stat_for(dir_meta);

        // Cascade + effective options for files directly in this dir.
        let dir_cascade = cascade_for_dir(
            self.root,
            dir_path,
            &mut self.config_cache,
            &mut self.cascade_cache,
        );
        let dir_effective = dir_cascade.effective();

        // Enumerate children: from the in-memory cache when the dir's
        // own mtime is unchanged, otherwise via a real readdir.
        let unchanged_names = self
            .prev_stats
            .get(dir_rel)
            .map(|p| p.mtime_ns == dir_fresh.mtime_ns)
            .unwrap_or(false);
        let children: Vec<(String, String)> = if unchanged_names {
            self.counters
                .dirs_readdir_skipped
                .fetch_add(1, Ordering::Relaxed);
            self.prev_children
                .get(dir_rel)
                .map(|kids| {
                    kids.iter()
                        .map(|child_rel| {
                            let name = child_rel
                                .rsplit('/')
                                .next()
                                .unwrap_or(child_rel)
                                .to_string();
                            (name, child_rel.clone())
                        })
                        .collect()
                })
                .unwrap_or_default()
        } else {
            match self.read_children(dir_path, dir_rel) {
                Ok(v) => v,
                Err(e) => {
                    self.counters.stat_errors.fetch_add(1, Ordering::Relaxed);
                    self.errors.push(WalkerError {
                        id: dir_rel.to_string(),
                        message: format!("readdir: {e}"),
                    });
                    Vec::new()
                }
            }
        };

        let mut tree_children: Vec<TreeChild> = Vec::with_capacity(children.len());
        let mut dir_size: i64 = 0;

        for (name, child_rel) in children {
            let child_path = dir_path.join(&name);
            let meta = match std::fs::symlink_metadata(&child_path) {
                Ok(m) => m,
                Err(e) => {
                    // A cached child that has vanished (or any stat
                    // failure): with a truncate-and-rebuild, simply not
                    // emitting its row is the deletion. NotFound on the
                    // skip path is benign; anything else is a real error.
                    if e.kind() != std::io::ErrorKind::NotFound {
                        self.counters.stat_errors.fetch_add(1, Ordering::Relaxed);
                        self.errors.push(WalkerError {
                            id: child_rel.clone(),
                            message: format!("stat: {e}"),
                        });
                    }
                    continue;
                }
            };

            let kind = if meta.file_type().is_symlink() {
                FileKind::Symlink
            } else if meta.is_dir() {
                FileKind::Dir
            } else if meta.is_file() {
                FileKind::File
            } else {
                continue;
            };

            // Ignore filter, with the same semantics as before: a dir is
            // tested against its own cascade (which includes its own
            // breadcrumb frame); a file/symlink against this dir's.
            let ignored = if matches!(kind, FileKind::Dir) {
                let eff = cascade_for_dir(
                    self.root,
                    &child_path,
                    &mut self.config_cache,
                    &mut self.cascade_cache,
                )
                .effective();
                matches_ignore(&eff, &child_rel, true, self.root)
            } else {
                matches_ignore(&dir_effective, &child_rel, false, self.root)
            };
            if ignored {
                self.counters
                    .ignored_entries
                    .fetch_add(1, Ordering::Relaxed);
                continue;
            }

            let fresh = fresh_stat_for(&meta);
            let (size, blake3, symlink_target): (i64, Blake3, Option<String>) = match kind {
                FileKind::File => {
                    self.counters.files_visited.fetch_add(1, Ordering::Relaxed);
                    let cached_hash = self.prev_blake3s.get(&child_rel);
                    let reuse = cached_hash.is_some()
                        && matches!(
                            stamp::decide(self.prev_stats.get(&child_rel), &fresh),
                            StampDecision::ReuseHash
                        );
                    if reuse {
                        self.summary.reused += 1;
                        self.counters.files_reused.fetch_add(1, Ordering::Relaxed);
                        self.counters
                            .bytes_skipped_cache
                            .fetch_add(meta.len(), Ordering::Relaxed);
                        (meta.len() as i64, *cached_hash.unwrap(), None)
                    } else {
                        self.summary.rehashed += 1;
                        self.counters.files_rehashed.fetch_add(1, Ordering::Relaxed);
                        self.counters
                            .bytes_hashed
                            .fetch_add(meta.len(), Ordering::Relaxed);
                        match hash_file(&child_path, meta.len()) {
                            Ok(h) => (meta.len() as i64, h, None),
                            Err(e) => {
                                self.counters.read_errors.fetch_add(1, Ordering::Relaxed);
                                self.errors.push(WalkerError {
                                    id: child_rel.clone(),
                                    message: format!("hash: {e:#}"),
                                });
                                continue;
                            }
                        }
                    }
                }
                FileKind::Symlink => {
                    self.counters
                        .symlinks_visited
                        .fetch_add(1, Ordering::Relaxed);
                    let target = match std::fs::read_link(&child_path) {
                        Ok(t) => t,
                        Err(e) => {
                            self.counters.read_errors.fetch_add(1, Ordering::Relaxed);
                            self.errors.push(WalkerError {
                                id: child_rel.clone(),
                                message: format!("read_link: {e}"),
                            });
                            continue;
                        }
                    };
                    let target_bytes = target.as_os_str().to_string_lossy();
                    let hex = hash_symlink_target(target_bytes.as_bytes());
                    self.summary.rehashed += 1;
                    (
                        target_bytes.len() as i64,
                        hex,
                        Some(target_bytes.into_owned()),
                    )
                }
                FileKind::Dir => {
                    let (h, sz) = self.walk_dir(&child_path, &child_rel, &meta)?;
                    (sz, h, None)
                }
            };

            tree_children.push(TreeChild {
                name: name.as_bytes().to_vec(),
                kind,
                blake3,
            });
            dir_size += size;

            // Files and symlinks emit their own row here; a dir already
            // emitted its row inside the recursive call above.
            if !matches!(kind, FileKind::Dir) {
                let stamp_kind = match kind {
                    FileKind::File => self.default_stamp_kind,
                    _ => StampKind::NoStamp,
                };
                let file_row = FileRow {
                    id: child_rel.clone(),
                    kind,
                    size,
                    blake3,
                    symlink_target,
                    identity_uuid: None,
                };
                let stat_row = FileStatsRow {
                    id: child_rel,
                    mtime_ns: fresh.mtime_ns,
                    size,
                    stamp_kind,
                    inode: if matches!(stamp_kind, StampKind::Inode) {
                        fresh.inode
                    } else {
                        None
                    },
                    dev: if matches!(stamp_kind, StampKind::Inode) {
                        fresh.dev
                    } else {
                        None
                    },
                    ctime_ns: fresh.ctime_ns,
                };
                self.push_row(file_row, stat_row)?;
            }
        }

        // This directory's own row (post-order: after every child).
        let dir_hash = hash_tree(&tree_children);
        let identity_uuid = dir_cascade
            .frame_for(dir_path)
            .and_then(|y| y.identity.as_ref().map(|i| i.uuid.clone()));
        self.summary.rehashed += 1;
        let file_row = FileRow {
            id: dir_rel.to_string(),
            kind: FileKind::Dir,
            size: dir_size,
            blake3: dir_hash,
            symlink_target: None,
            identity_uuid,
        };
        let stat_row = FileStatsRow {
            id: dir_rel.to_string(),
            mtime_ns: dir_fresh.mtime_ns,
            size: dir_size,
            stamp_kind: StampKind::NoStamp,
            inode: None,
            dev: None,
            ctime_ns: dir_fresh.ctime_ns,
        };
        self.push_row(file_row, stat_row)?;

        Ok((dir_hash, dir_size))
    }

    /// Real `readdir`: the directory's current entries, skipping the
    /// breadcrumb file and non-utf8 names, sorted by name for stable
    /// output (matching the old `walkdir` `sort_by_file_name`).
    fn read_children(
        &self,
        dir_path: &Path,
        dir_rel: &str,
    ) -> std::io::Result<Vec<(String, String)>> {
        let mut out: Vec<(String, String)> = Vec::new();
        for entry in std::fs::read_dir(dir_path)? {
            let entry = entry?;
            let name_os = entry.file_name();
            if name_os == BREADCRUMB_FILENAME {
                continue;
            }
            let Some(name) = name_os.to_str() else {
                self.counters.non_utf8_paths.fetch_add(1, Ordering::Relaxed);
                warn!(
                    event = "fsindex_skip_non_utf8_path",
                    path = %dir_path.join(&name_os).display(),
                );
                continue;
            };
            let name = name.to_string();
            let child_rel = if dir_rel.is_empty() {
                name.clone()
            } else {
                format!("{dir_rel}/{name}")
            };
            out.push((name, child_rel));
        }
        out.sort();
        Ok(out)
    }
}

/// Accumulated options cascade for a directory (`root` → `dir`),
/// memoized so each `.fsindex.yaml` is read at most once per scan and
/// each directory's cascade is built at most once.
///
/// `config_cache` holds the parsed-or-absent breadcrumb per directory;
/// `cascade_cache` holds the cumulative cascade per directory. Because
/// a directory's cascade is just its parent's cascade plus its own
/// breadcrumb frame, building it walks down from the nearest cached
/// ancestor, so steady-state cost is one hash lookup per ancestor and
/// zero filesystem reads. This replaces the old per-entry rebuild that
/// re-read every ancestor breadcrumb for every file.
fn cascade_for_dir(
    root: &Path,
    dir: &Path,
    config_cache: &mut HashMap<PathBuf, Option<FsindexYaml>>,
    cascade_cache: &mut HashMap<PathBuf, OptionsCascade>,
) -> OptionsCascade {
    if let Some(cached) = cascade_cache.get(dir) {
        return cached.clone();
    }
    let mut cascade = OptionsCascade::new();
    for d in ancestor_chain(root, dir) {
        // Restart from the deepest already-built ancestor when we have
        // one, then extend downward for the rest.
        if let Some(cached) = cascade_cache.get(&d) {
            cascade = cached.clone();
            continue;
        }
        let yaml = config_cache
            .entry(d.clone())
            .or_insert_with(|| match options::load_at(&d) {
                Ok(y) => y,
                Err(err) => {
                    warn!(
                        event = "fsindex_options_parse_error",
                        dir = %d.display(),
                        error = %err,
                    );
                    None
                }
            });
        if let Some(y) = yaml {
            cascade.push(d.clone(), y.clone());
        }
        cascade_cache.insert(d.clone(), cascade.clone());
    }
    cascade
}

fn ancestor_chain(root: &Path, entry: &Path) -> Vec<PathBuf> {
    let mut chain: Vec<PathBuf> = Vec::new();
    if entry == root {
        chain.push(root.to_path_buf());
        return chain;
    }
    let Ok(rel) = entry.strip_prefix(root) else {
        return vec![root.to_path_buf()];
    };
    let mut cur = root.to_path_buf();
    chain.push(cur.clone());
    for part in rel.iter() {
        cur.push(part);
        // Don't include the entry itself unless it's a directory; we
        // overshoot here by one and let the caller's `frame_for`
        // check handle dir-specific frames. For files this means we
        // also pushed the file's own path as a frame which load_at
        // will return None for — harmless.
        chain.push(cur.clone());
    }
    chain
}

fn fresh_stat_for(meta: &std::fs::Metadata) -> FreshStat {
    let mtime_ns = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0);
    let ctime_ns = meta
        .created()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64);
    let (inode, dev) = unix_inode_dev(meta);
    FreshStat {
        mtime_ns,
        size: meta.len() as i64,
        inode,
        dev,
        ctime_ns,
    }
}

#[cfg(unix)]
fn unix_inode_dev(meta: &std::fs::Metadata) -> (Option<i64>, Option<i64>) {
    use std::os::unix::fs::MetadataExt;
    (Some(meta.ino() as i64), Some(meta.dev() as i64))
}

#[cfg(not(unix))]
fn unix_inode_dev(_meta: &std::fs::Metadata) -> (Option<i64>, Option<i64>) {
    (None, None)
}

/// Gitignore-shaped matcher backed by the `ignore` crate (same
/// implementation ripgrep uses), so full gitignore semantics —
/// `**`, anchored `/`, negation with `!`, comments, character
/// classes — are correctly handled. Patterns are interpreted as
/// rooted at the scan root regardless of which cascade level
/// wrote them; that means a pattern in a nested `.fsindex.yaml`
/// applies globally rather than being anchored to its own
/// directory, which is a deliberate simplification of gitignore
/// semantics for fsindex's cascaded-config model.
/// FIXME(ignore-perf): builds a `Gitignore` per entry; at 50M
/// rows we'll want to cache by cascade-frame identity.
fn matches_ignore(eff: &EffectiveOptions, rel: &str, is_dir: bool, root: &Path) -> bool {
    let mut b = ignore::gitignore::GitignoreBuilder::new(root);
    for line in &eff.ignore_patterns {
        let _ = b.add_line(None, line);
    }
    let Ok(gi) = b.build() else {
        return false;
    };
    let candidate = root.join(rel);
    matches!(
        gi.matched_path_or_any_parents(&candidate, is_dir),
        ignore::Match::Ignore(_)
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(path: &Path, bytes: &[u8]) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, bytes).unwrap();
    }

    /// Rebuild the in-memory rescan cache from a prior walk's results,
    /// mirroring `db::load_prev_cache` so these tests exercise the
    /// walker (and its readdir-skip) in isolation from the database.
    #[allow(clippy::type_complexity)]
    fn build_cache(
        rows: &[ScanResult],
    ) -> (
        HashMap<String, FileStatsRow>,
        HashMap<String, Blake3>,
        HashMap<String, Vec<String>>,
    ) {
        let mut stats = HashMap::new();
        let mut blake3s = HashMap::new();
        let mut children: HashMap<String, Vec<String>> = HashMap::new();
        for r in rows {
            stats.insert(r.stat_row.id.clone(), r.stat_row.clone());
            if matches!(r.file_row.kind, FileKind::File) {
                blake3s.insert(r.file_row.id.clone(), r.file_row.blake3);
            }
        }
        for id in stats.keys() {
            if id.is_empty() {
                continue;
            }
            let parent = match id.rfind('/') {
                Some(i) => id[..i].to_string(),
                None => String::new(),
            };
            children.entry(parent).or_default().push(id.clone());
        }
        for kids in children.values_mut() {
            kids.sort();
        }
        (stats, blake3s, children)
    }

    fn walk(
        root: &Path,
        stats: &HashMap<String, FileStatsRow>,
        blake3s: &HashMap<String, Blake3>,
        children: &HashMap<String, Vec<String>>,
    ) -> (Vec<ScanResult>, WalkerCounters) {
        let stamp_kind = if cfg!(unix) {
            StampKind::Inode
        } else {
            StampKind::NoStamp
        };
        let counters = WalkerCounters::default();
        let mut out = Vec::new();
        let walker = Walker::new(root, stats, blake3s, children, stamp_kind);
        walker
            .collect_streaming(&counters, |b| {
                out.extend(b);
                Ok(())
            })
            .unwrap();
        (out, counters)
    }

    fn blake_of(rows: &[ScanResult], id: &str) -> Option<Blake3> {
        rows.iter()
            .find(|r| r.file_row.id == id)
            .map(|r| r.file_row.blake3)
    }
    fn has(rows: &[ScanResult], id: &str) -> bool {
        rows.iter().any(|r| r.file_row.id == id)
    }

    /// The readdir-skip fast path: an unchanged directory enumerates its
    /// children from the cache (no readdir) — but still `lstat`s each
    /// child, so an in-place content edit is caught regardless.
    #[test]
    fn readdir_skip_fires_and_still_catches_in_place_edits() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(&root.join("a.txt"), b"aaa");
        write(&root.join("b.txt"), b"bbb");
        write(&root.join("sub/c.txt"), b"ccc");

        // Cold scan: no cache, so nothing can be skipped.
        let (rows1, c1) = walk(root, &HashMap::new(), &HashMap::new(), &HashMap::new());
        assert_eq!(
            c1.dirs_readdir_skipped.load(Ordering::Relaxed),
            0,
            "a cold scan has no cache to skip against"
        );
        assert_eq!(
            c1.dirs_visited.load(Ordering::Relaxed),
            2,
            "root + sub are both visited"
        );

        let (stats, blake3s, kids) = build_cache(&rows1);

        // Unchanged rescan: both dirs match their cached mtime, so both
        // skip readdir, and every file reuses its cached hash.
        let (rows2, c2) = walk(root, &stats, &blake3s, &kids);
        assert_eq!(
            c2.dirs_readdir_skipped.load(Ordering::Relaxed),
            2,
            "both unchanged dirs skip the readdir syscall"
        );
        assert_eq!(
            c2.files_reused.load(Ordering::Relaxed),
            3,
            "every unchanged file reuses its cached blake3"
        );
        assert_eq!(
            c2.files_rehashed.load(Ordering::Relaxed),
            0,
            "no file is rehashed on a fully-unchanged rescan"
        );
        for id in ["a.txt", "b.txt", "sub/c.txt"] {
            assert_eq!(
                blake_of(&rows2, id),
                blake_of(&rows1, id),
                "{id} output must be identical across the rescan"
            );
        }

        // Edit a.txt in place (content + length change). Editing a file's
        // contents does NOT change its parent directory's mtime, so root's
        // readdir is STILL skipped — yet the edit must be caught because
        // the skip path lstats every cached child.
        write(&root.join("a.txt"), b"aaaa-now-longer");
        let (rows3, c3) = walk(root, &stats, &blake3s, &kids);
        assert_eq!(
            c3.dirs_readdir_skipped.load(Ordering::Relaxed),
            2,
            "an in-place file edit leaves dir mtimes unchanged, so readdir is still skipped"
        );
        assert_ne!(
            blake_of(&rows3, "a.txt"),
            blake_of(&rows1, "a.txt"),
            "the in-place edit MUST be caught despite the readdir skip"
        );
        assert_eq!(
            blake_of(&rows3, "b.txt"),
            blake_of(&rows1, "b.txt"),
            "an untouched sibling still reuses its hash"
        );
        assert!(
            c3.files_rehashed.load(Ordering::Relaxed) >= 1,
            "the edited file is rehashed"
        );
    }

    // ╔════════════════════════════════════════════════════════════════╗
    // ║  KNOWN BUG — PINNED, NOT YET FIXED. DO NOT READ AS CORRECT.     ║
    // ║                                                                ║
    // ║  Loosening a cascade `ignore` rule by editing an EXISTING       ║
    // ║  `.fsindex.yaml` does not change that directory's mtime, so the ║
    // ║  readdir-skip fast path reuses the cached child list — which    ║
    // ║  never contained the previously-ignored entry. The newly-      ║
    // ║  unignored file is therefore SILENTLY NOT INDEXED until the     ║
    // ║  directory's mtime next changes (or `--reset-and-redownload`).  ║
    // ║                                                                ║
    // ║  This test pins the WRONG behavior so we get a failure the day  ║
    // ║  it changes. The fix (e.g. a per-directory effective-options    ║
    // ║  fingerprint that forces a readdir when the ignore set drifts)  ║
    // ║  should FLIP the marked assertion to `assert!(has(...))`.       ║
    // ╚════════════════════════════════════════════════════════════════╝
    #[test]
    fn loosening_ignore_misses_newly_unignored_file_known_bug() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(&root.join(".fsindex.yaml"), b"ignore:\n  - '*.secret'\n");
        write(&root.join("keep.txt"), b"keep");
        write(&root.join("hidden.secret"), b"shhh");

        // Cold scan: hidden.secret is ignored, so it never enters the
        // cache we build from this scan.
        let (rows1, _c1) = walk(root, &HashMap::new(), &HashMap::new(), &HashMap::new());
        assert!(has(&rows1, "keep.txt"));
        assert!(
            !has(&rows1, "hidden.secret"),
            "hidden.secret is ignored on the cold scan"
        );
        let (stats, blake3s, kids) = build_cache(&rows1);

        // Loosen the rule by rewriting the EXISTING breadcrumb. This does
        // not change root's mtime, so the rescan takes the skip path.
        write(&root.join(".fsindex.yaml"), b"ignore: []\n");
        let (rows2, c2) = walk(root, &stats, &blake3s, &kids);
        assert!(
            c2.dirs_readdir_skipped.load(Ordering::Relaxed) >= 1,
            "root's mtime is unchanged, so its readdir is skipped — the cause of the bug"
        );

        // vvv WRONG-BEHAVIOR ASSERTION — flip to assert!(has(...)) when fixed vvv
        assert!(
            !has(&rows2, "hidden.secret"),
            "KNOWN BUG: hidden.secret is now un-ignored but stays missing, because \
             the readdir-skip reused the cached (pre-unignore) child list. If this \
             assertion just failed, the bug is FIXED — flip it to assert!(has(...))."
        );
        // ^^^ WRONG-BEHAVIOR ASSERTION ^^^

        // For contrast, the newly-*ignored* direction is handled correctly:
        // every enumerated child is re-tested against the current ignore set.
        write(&root.join(".fsindex.yaml"), b"ignore:\n  - 'keep.txt'\n");
        let (rows3, _c3) = walk(root, &stats, &blake3s, &kids);
        assert!(
            !has(&rows3, "keep.txt"),
            "a newly-ignored file IS correctly dropped, even on the skip path"
        );
    }

    /// Pokes the skip boundary on a 3-deep tree (root → mid → inner):
    /// a change inside `mid` must re-examine `mid` while the untouched
    /// `root` and `mid/inner` keep benefiting from the cache. Also pins
    /// the important distinction between the two kinds of change:
    ///   * a STRUCTURAL change (adding/removing/renaming an entry) bumps
    ///     the directory's mtime, so that directory is re-`readdir`ed;
    ///   * an in-place CONTENT edit does NOT change any directory's
    ///     mtime, so NO directory is re-`readdir`ed — the edit is caught
    ///     by the per-file `lstat` instead (a re-hash of the file, not a
    ///     re-readdir of its directory, which would be wasted work).
    #[test]
    fn change_in_a_dir_rescans_only_that_dir_inner_dirs_stay_cached() {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        write(&root.join("top.txt"), b"top");
        write(&root.join("mid/m.txt"), b"m");
        write(&root.join("mid/inner/i.txt"), b"i");

        let (rows1, _c1) = walk(root, &HashMap::new(), &HashMap::new(), &HashMap::new());
        let (stats, blake3s, kids) = build_cache(&rows1);

        // ── Structural change: add a file under `mid`. This bumps mid's
        // mtime, so mid is re-readdir'd and the new file is discovered.
        // `root` and `mid/inner` are untouched and skip their readdir.
        write(&root.join("mid/added.txt"), b"added");
        let (rows_add, c_add) = walk(root, &stats, &blake3s, &kids);
        assert!(
            has(&rows_add, "mid/added.txt"),
            "a newly-added file under mid must be discovered (mid is re-readdir'd)"
        );
        assert_eq!(
            c_add.dirs_visited.load(Ordering::Relaxed),
            3,
            "root, mid, inner are all visited"
        );
        assert_eq!(
            c_add.dirs_readdir_skipped.load(Ordering::Relaxed),
            2,
            "only `mid` is re-readdir'd; `root` and `mid/inner` still skip it"
        );
        assert_eq!(
            blake_of(&rows_add, "mid/inner/i.txt"),
            blake_of(&rows1, "mid/inner/i.txt"),
            "the untouched inner subtree stays fully cached"
        );

        // Re-baseline the cache to the post-add state (a normal next scan),
        // so the following edit is measured against an up-to-date cache.
        let (stats2, blake3s2, kids2) = build_cache(&rows_add);

        // ── Content edit: rewrite mid/m.txt in place. No directory's
        // mtime changes, so EVERY directory skips its readdir — yet the
        // edit is still caught because each cached child is lstat'd and
        // the moved (mtime,size) forces a re-hash.
        write(&root.join("mid/m.txt"), b"m-edited-and-longer");
        let (rows_edit, c_edit) = walk(root, &stats2, &blake3s2, &kids2);
        assert_eq!(
            c_edit.dirs_readdir_skipped.load(Ordering::Relaxed),
            3,
            "a content edit changes no dir mtime, so all 3 dirs skip readdir"
        );
        assert_ne!(
            blake_of(&rows_edit, "mid/m.txt"),
            blake_of(&rows_add, "mid/m.txt"),
            "the edited file is re-hashed (caught via lstat) despite the readdir skip"
        );
        assert_eq!(
            blake_of(&rows_edit, "mid/inner/i.txt"),
            blake_of(&rows1, "mid/inner/i.txt"),
            "the untouched inner subtree is still fully cached after the edit"
        );
        assert!(
            has(&rows_edit, "mid/added.txt"),
            "the previously-added file is still present"
        );
    }
}
