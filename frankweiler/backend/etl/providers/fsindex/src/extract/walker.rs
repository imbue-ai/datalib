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
//! CONCERN(perf-unmeasured): jwalk / `ignore` are claimed-fast but
//! we're using `walkdir` here (the simpler fallback the prompt
//! permits). Performance against the design-target tens-of-millions
//! scale is asserted, not measured.
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
use walkdir::WalkDir;

use super::hash::{hash_file, hash_symlink_target, hash_tree, Blake3, TreeChild};
use super::metrics::WalkerCounters;
use super::options::{self, EffectiveOptions, OptionsCascade, BREADCRUMB_FILENAME};
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
    default_stamp_kind: StampKind,
}

impl<'a> Walker<'a> {
    pub fn new(
        root: &'a Path,
        prev_stats: &'a HashMap<String, FileStatsRow>,
        prev_file_blake3s: &'a HashMap<String, Blake3>,
        default_stamp_kind: StampKind,
    ) -> Self {
        Self {
            root,
            prev_stats,
            prev_file_blake3s,
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
        mut emit_batch: F,
    ) -> Result<(Vec<WalkerError>, WalkerSummary)>
    where
        F: FnMut(Vec<ScanResult>) -> Result<()>,
    {
        let root_canonical = self.root.to_path_buf();
        let mut errors: Vec<WalkerError> = Vec::new();
        let mut summary = WalkerSummary {
            rehashed: 0,
            reused: 0,
        };

        let mut buf: Vec<ScanResult> = Vec::with_capacity(BATCH_SIZE);
        let mut dir_children: HashMap<PathBuf, Vec<TreeChild>> = HashMap::new();
        let mut dir_sizes: HashMap<PathBuf, i64> = HashMap::new();
        let mut dir_visible: HashMap<PathBuf, bool> = HashMap::new();

        let mut cascade = OptionsCascade::new();
        let mut cascade_pushed: HashMap<PathBuf, bool> = HashMap::new();
        // Cascade is built lazily on a per-entry basis: when emitting
        // a file/symlink we resolve which ancestor cascade frames
        // apply by walking up the path. The simpler alternative
        // (push-on-descent) doesn't fit `contents_first=true` cleanly.

        let walker_iter = WalkDir::new(&root_canonical)
            .follow_links(false)
            .contents_first(true)
            .sort_by_file_name();

        for entry_result in walker_iter {
            let entry = match entry_result {
                Ok(e) => e,
                Err(err) => {
                    counters.stat_errors.fetch_add(1, Ordering::Relaxed);
                    warn!(event = "fsindex_walk_entry_error", error = %err);
                    continue;
                }
            };
            let entry = &entry;
            let path = entry.path();
            let is_root = path == root_canonical;
            let rel = if is_root {
                String::new()
            } else {
                match path.strip_prefix(&root_canonical) {
                    Ok(r) => match r.to_str() {
                        Some(s) => s.replace('\\', "/"),
                        None => {
                            counters.non_utf8_paths.fetch_add(1, Ordering::Relaxed);
                            warn!(
                                event = "fsindex_skip_non_utf8_path",
                                path = %r.display(),
                            );
                            errors.push(WalkerError {
                                id: r.to_string_lossy().to_string(),
                                message: "non-utf8 path".to_string(),
                            });
                            continue;
                        }
                    },
                    Err(_) => continue,
                }
            };

            // Skip breadcrumb files entirely — they're scanner
            // metadata, not content. (Schema doc: excluded from
            // tree-hash AND from `files`.)
            if entry.file_name() == BREADCRUMB_FILENAME {
                continue;
            }

            let meta = match entry.metadata() {
                Ok(m) => m,
                Err(err) => {
                    counters.stat_errors.fetch_add(1, Ordering::Relaxed);
                    errors.push(WalkerError {
                        id: rel.clone(),
                        message: format!("stat: {err}"),
                    });
                    continue;
                }
            };

            // Build / refresh the cascade for this entry's parent
            // chain. We rebuild from scratch per entry — cheap
            // relative to I/O, and correct under any walk order.
            let cascade_for_entry = self.build_cascade(&root_canonical, path);
            let effective = cascade_for_entry.effective();

            // Decide kind.
            let kind = if meta.file_type().is_symlink() {
                FileKind::Symlink
            } else if meta.is_dir() {
                FileKind::Dir
            } else if meta.is_file() {
                FileKind::File
            } else {
                continue;
            };

            // Ignore filter. Skip from `files` AND from parent's
            // tree-hash. Root is never ignored even if user wrote a
            // matching pattern.
            if !is_root
                && matches_ignore(
                    &effective,
                    &rel,
                    matches!(kind, FileKind::Dir),
                    &root_canonical,
                )
            {
                counters.ignored_entries.fetch_add(1, Ordering::Relaxed);
                continue;
            }

            let fresh = fresh_stat_for(&meta);

            let (size, blake3, symlink_target_str) = match kind {
                FileKind::File => {
                    counters.files_visited.fetch_add(1, Ordering::Relaxed);
                    let prev = self.prev_stats.get(&rel);
                    let decision = stamp::decide(prev, &fresh);
                    // Reuse only if stamp::decide says so AND we have a
                    // cached blake3 to actually reuse. Either gate
                    // failing falls back to Rehash; this is the
                    // RescanStamp-on-startup case from Unison.
                    let hex = match decision {
                        StampDecision::ReuseHash => match self.prev_file_blake3s.get(&rel) {
                            Some(cached) => {
                                summary.reused += 1;
                                counters.files_reused.fetch_add(1, Ordering::Relaxed);
                                counters
                                    .bytes_skipped_cache
                                    .fetch_add(meta.len(), Ordering::Relaxed);
                                *cached
                            }
                            None => {
                                summary.rehashed += 1;
                                counters.files_rehashed.fetch_add(1, Ordering::Relaxed);
                                counters
                                    .bytes_hashed
                                    .fetch_add(meta.len(), Ordering::Relaxed);
                                match hash_file(path, meta.len()) {
                                    Ok(h) => h,
                                    Err(e) => {
                                        counters.read_errors.fetch_add(1, Ordering::Relaxed);
                                        errors.push(WalkerError {
                                            id: rel.clone(),
                                            message: format!("hash: {e:#}"),
                                        });
                                        continue;
                                    }
                                }
                            }
                        },
                        StampDecision::Rehash => {
                            summary.rehashed += 1;
                            counters.files_rehashed.fetch_add(1, Ordering::Relaxed);
                            counters
                                .bytes_hashed
                                .fetch_add(meta.len(), Ordering::Relaxed);
                            match hash_file(path, meta.len()) {
                                Ok(h) => h,
                                Err(e) => {
                                    counters.read_errors.fetch_add(1, Ordering::Relaxed);
                                    errors.push(WalkerError {
                                        id: rel.clone(),
                                        message: format!("hash: {e:#}"),
                                    });
                                    continue;
                                }
                            }
                        }
                    };
                    (meta.len() as i64, hex, None)
                }
                FileKind::Symlink => {
                    counters.symlinks_visited.fetch_add(1, Ordering::Relaxed);
                    let target = std::fs::read_link(path)
                        .with_context(|| format!("read_link {}", path.display()))?;
                    let target_bytes = target.as_os_str().to_string_lossy();
                    let hex = hash_symlink_target(target_bytes.as_bytes());
                    summary.rehashed += 1;
                    (
                        target_bytes.len() as i64,
                        hex,
                        Some(target_bytes.into_owned()),
                    )
                }
                FileKind::Dir => {
                    counters.dirs_visited.fetch_add(1, Ordering::Relaxed);
                    let kids = dir_children.remove(path).unwrap_or_default();
                    let hex = hash_tree(&kids);
                    let size = dir_sizes.remove(path).unwrap_or(0);
                    summary.rehashed += 1;
                    (size, hex, None)
                }
            };

            // Identity uuid: dir only, from currently-loaded breadcrumb.
            let identity_uuid = if matches!(kind, FileKind::Dir) {
                cascade_for_entry
                    .frame_for(path)
                    .and_then(|y| y.identity.as_ref().map(|i| i.uuid.clone()))
            } else {
                None
            };

            // Skip emitting a row for the root with an empty `id`
            // (root is the source itself; `scan_meta` holds the
            // per-source state). The dir-hash for the root is
            // discarded; we still want children to roll up to it.
            // ACTUALLY: we DO want a row for the root so
            // tree-equality is queryable at the top. Use id="" if
            // root; or use "." as the root id. We pick "" to match
            // EXTRACT.md's posix-relative-no-leading-slash rule and
            // accept that the root row has the empty-string PK.
            let id_to_use = if is_root { String::new() } else { rel.clone() };

            // Roll this entry up into parent dir's children list and
            // size, unless it's the root.
            if !is_root {
                if let Some(parent) = path.parent() {
                    let kind_for_tree = kind;
                    let name_bytes = entry.file_name().to_string_lossy().as_bytes().to_vec();
                    dir_children
                        .entry(parent.to_path_buf())
                        .or_default()
                        .push(TreeChild {
                            name: name_bytes,
                            kind: kind_for_tree,
                            blake3,
                        });
                    *dir_sizes.entry(parent.to_path_buf()).or_insert(0) += size;
                    dir_visible.insert(parent.to_path_buf(), true);
                }
            }

            let stamp_kind = match kind {
                FileKind::File => self.default_stamp_kind,
                _ => StampKind::NoStamp,
            };

            let file_row = FileRow {
                id: id_to_use.clone(),
                kind,
                size,
                blake3,
                symlink_target: symlink_target_str,
                identity_uuid,
            };
            let stat_row = FileStatsRow {
                id: id_to_use,
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
            buf.push(ScanResult { file_row, stat_row });
            counters.rows_emitted.fetch_add(1, Ordering::Relaxed);

            // Silence dead-stores in the suppressor maps for entries
            // we processed but didn't recurse into.
            let _ = &mut cascade;
            let _ = &mut cascade_pushed;
            let _ = &mut dir_visible;

            if buf.len() >= BATCH_SIZE {
                counters.batches_emitted.fetch_add(1, Ordering::Relaxed);
                let drained = std::mem::replace(&mut buf, Vec::with_capacity(BATCH_SIZE));
                emit_batch(drained)?;
            }
        }

        if !buf.is_empty() {
            counters.batches_emitted.fetch_add(1, Ordering::Relaxed);
            emit_batch(buf)?;
        }

        Ok((errors, summary))
    }

    /// Build a fresh cascade for an entry by loading every
    /// ancestor's `.fsindex.yaml` from the root down to (and
    /// including) the entry's directory.
    fn build_cascade(&self, root: &Path, entry_path: &Path) -> OptionsCascade {
        let mut cascade = OptionsCascade::new();
        let chain = ancestor_chain(root, entry_path);
        for dir in chain {
            match options::load_at(&dir) {
                Ok(Some(y)) => cascade.push(dir, y),
                Ok(None) => {}
                Err(err) => {
                    warn!(
                        event = "fsindex_options_parse_error",
                        dir = %dir.display(),
                        error = %err,
                    );
                }
            }
        }
        cascade
    }
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
