//! A host-local cache of "what did this path look like, and what was its
//! hash" — the fast-rescan cursor, kept out of versioned storage.
//!
//! # Why this is not a table in the scan store
//!
//! Every tree-scanning provider keeps a Unison-style cursor so a rescan
//! can skip hashing a file whose `(mtime, size, inode, dev)` has not
//! moved. Until now each kept it inside its own `.doltlite_db`, beside
//! the content. That is the wrong home for three reasons, and the third
//! is the one that bites:
//!
//! 1. **It is host state.** An inode number means nothing on another
//!    machine. A branch fetched from elsewhere carries a cursor that
//!    cannot match, so every file rehashes — and nothing records which
//!    host a cursor came from, so you cannot even detect it.
//! 2. **Branching it is a category error.** The cursor describes the
//!    *live filesystem*, which has no history. Rolling a branch back
//!    does not un-modify the files on disk, so a rolled-back cursor
//!    would be describing a machine state that never existed. Per host
//!    there is only ever a latest.
//! 3. **A fresh branch loses it.** Start a new branch of the scan data
//!    and the cursor is gone with it, so a rescan of an unchanged tree
//!    pays a full rehash for a reason that has nothing to do with the
//!    tree.
//!
//! Measured, 100k entries: `files` + `file_stats` in one doltlite store
//! is 291 B/row; `files` alone is 148 B/row. **The cursor was 49% of the
//! versioned store**, because `file_stats` re-stores the full path as
//! its own primary key.
//!
//! # Why plain SQLite
//!
//! This is a cache: losing it costs a rehash, not correctness. It needs
//! no commits, no history, and no prolly tree. doltlite creates its own
//! `CTLD` format by default, but the `doltlite_engine=sqlite` URI
//! parameter opts out for a new empty file — the same door
//! `datalib_progress::bus` goes through. Measured on 100k rows: bulk
//! write 0.20s against 0.37s (insert + `dolt_commit` + `dolt_gc`), and
//! single-row updates ~0.3ms against ~50ms.
//!
//! # Keyed by absolute path
//!
//! One chain per host, not per root — which is the part Unison gets
//! wrong. Its `fpcache` is per replica *pair*, so syncing one tree
//! against two peers hashes the same bytes twice, and scanning a
//! directory tells you nothing about its parent.
//!
//! Absolute keys make overlapping roots share work in both directions.
//! Measured, an inner directory of 2000 files inside a parent that also
//! holds 500 of its own:
//!
//! | | files reused | files hashed |
//! |---|---|---|
//! | scan the inner directory (cold) | 0 | 2000 |
//! | then scan the **parent** | **2000** | 500 (only its own) |
//! | then scan the inner directory again | 2000 | 0 |
//!
//! Pruning stays scoped to the root it was given, so the parent's
//! entries survive a nested scan and vice versa — otherwise two
//! overlapping scans would keep evicting each other and neither would
//! ever be fast.
//!
//! A root that moves simply misses rather than colliding, and two
//! providers scanning one tree reuse each other's work.
//!
//! # Removal is by "the file is gone", never by "I did not look at it"
//!
//! Two different things look alike from inside one scan, and
//! conflating them breaks the cache in opposite directions:
//!
//! - **The path no longer exists.** Dead weight. It should go, and a
//!   scan that covered the directory is exactly who knows.
//! - **The scan did not look at the path.** It must stay. What a scan
//!   sees is a property of *its filters*, not of the filesystem, and
//!   this cache is shared by consumers who disagree about what is
//!   interesting: `fsindex` honours a per-directory `ignore` cascade,
//!   `pdf` only wants PDFs.
//!
//! The first version pruned on "the scan did not write a row for it",
//! which is the second thing wearing the first thing's clothes — so an
//! `fsindex` scan ignoring `*.tmp` evicted 200 entries a full scan had
//! just cached. The distinction is settled with one `lstat` per
//! candidate: gone means gone, and anything still on disk is kept
//! whatever this scan's filters thought of it.
//!
//! [`FingerprintCache::forget`] is therefore deliberately dumb — it
//! removes exactly what it is handed. The policy lives in the caller,
//! which is the only party that can tell the two cases apart.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use crate::fswalk::{Blake3, StampCursor, StampKind};

/// What kind of thing a cached entry describes.
///
/// Directories are cached too: `fsindex` hashes a directory over its
/// children, so a directory has a digest like anything else, and its
/// cursor is what lets a rescan skip the `readdir`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Dir,
    Symlink,
}

impl EntryKind {
    pub fn as_str(self) -> &'static str {
        match self {
            EntryKind::File => "file",
            EntryKind::Dir => "dir",
            EntryKind::Symlink => "symlink",
        }
    }

    /// Unknown strings become [`EntryKind::File`]. A cache row we cannot
    /// interpret is not worth failing a scan over — the worst case is a
    /// rehash.
    pub fn from_str_or_file(s: &str) -> Self {
        match s {
            "dir" => EntryKind::Dir,
            "symlink" => EntryKind::Symlink,
            _ => EntryKind::File,
        }
    }
}

/// One cached observation: a path, what it was, and what it hashed to.
#[derive(Debug, Clone)]
pub struct Fingerprint {
    /// Absolute path. Callers canonicalize the *root*; the walker joins
    /// relative paths onto it, so a symlinked component inside the tree
    /// is recorded as walked rather than resolved.
    pub abs_path: String,
    pub kind: EntryKind,
    /// Content hash for a file, link-target hash for a symlink, tree
    /// hash for a directory.
    pub blake3: Blake3,
    pub cursor: StampCursor,
}

/// The cache rows under one root, keyed by **root-relative** path so a
/// provider's walker can use them without knowing where the root is.
#[derive(Debug, Default)]
pub struct CachedTree {
    entries: HashMap<String, (EntryKind, Blake3, StampCursor)>,
    children: HashMap<String, Vec<String>>,
}

impl CachedTree {
    /// Build a tree from root-relative entries held in memory.
    ///
    /// The child index is derived here, so a caller never has to keep
    /// the two in step. Used by tests, and by any provider that wants a
    /// prior view from somewhere other than the cache file.
    pub fn from_entries(
        items: impl IntoIterator<Item = (String, EntryKind, Blake3, StampCursor)>,
    ) -> Self {
        let mut tree = CachedTree {
            entries: items
                .into_iter()
                .map(|(rel, kind, hash, cursor)| (rel, (kind, hash, cursor)))
                .collect(),
            children: HashMap::new(),
        };
        tree.index_children();
        tree
    }

    /// The stat cursor recorded for a root-relative path.
    pub fn cursor(&self, rel: &str) -> Option<&StampCursor> {
        self.entries.get(rel).map(|(_, _, c)| c)
    }

    /// The digest recorded for a root-relative path.
    pub fn blake3(&self, rel: &str) -> Option<Blake3> {
        self.entries.get(rel).map(|(_, h, _)| *h)
    }

    pub fn kind(&self, rel: &str) -> Option<EntryKind> {
        self.entries.get(rel).map(|(k, _, _)| *k)
    }

    /// The immediate children recorded for a directory, root-relative
    /// and sorted. The root's children are keyed by the empty string.
    ///
    /// Derived from the key set rather than stored, so it cannot drift
    /// out of step with the entries themselves.
    pub fn children(&self, rel: &str) -> Option<&Vec<String>> {
        self.children.get(rel)
    }

    /// Every root-relative path in the tree, unordered.
    pub fn paths(&self) -> impl Iterator<Item = &String> {
        self.entries.keys()
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    fn index_children(&mut self) {
        let mut kids: HashMap<String, Vec<String>> = HashMap::new();
        for rel in self.entries.keys() {
            if rel.is_empty() {
                continue;
            }
            let parent = match rel.rfind('/') {
                Some(i) => rel[..i].to_string(),
                None => String::new(),
            };
            kids.entry(parent).or_default().push(rel.clone());
        }
        for v in kids.values_mut() {
            v.sort_unstable();
        }
        self.children = kids;
    }
}

pub const SCHEMA: &str = "CREATE TABLE IF NOT EXISTS fingerprints (
    abs_path    TEXT PRIMARY KEY,
    kind        TEXT NOT NULL,
    blake3      BLOB NOT NULL,
    mtime_ns    INTEGER NOT NULL,
    size        INTEGER NOT NULL,
    stamp_kind  TEXT NOT NULL,
    inode       INTEGER,
    dev         INTEGER
)";

/// The default cache location for this host.
///
/// A cache directory, deliberately — not the data root. Host state in a
/// data root is wrong twice: the root may sit on a synced volume (this
/// repo lives under Dropbox), which would replicate one machine's inode
/// numbers to another; and a data root is a thing you copy or move,
/// while this describes the machine.
pub fn default_cache_path() -> Result<PathBuf> {
    if let Some(dir) = std::env::var_os("DATALIB_CACHE_DIR") {
        return Ok(PathBuf::from(dir).join("fingerprints.sqlite"));
    }
    if let Some(dir) = std::env::var_os("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(dir)
            .join("datalib")
            .join("fingerprints.sqlite"));
    }
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .context("neither DATALIB_CACHE_DIR, XDG_CACHE_HOME nor HOME is set")?;
    let base = if cfg!(target_os = "macos") {
        home.join("Library").join("Caches")
    } else {
        home.join(".cache")
    };
    Ok(base.join("datalib").join("fingerprints.sqlite"))
}

fn connect_string(path: &Path) -> String {
    // SQLite percent-decodes a URI's path, so anything that would
    // terminate it or be decoded away has to be escaped. Spaces are
    // fine and are left alone — data roots have them.
    let escaped = path
        .display()
        .to_string()
        .replace('%', "%25")
        .replace('?', "%3f")
        .replace('#', "%23");
    format!("file:{escaped}?doltlite_engine=sqlite")
}

/// A host-local fingerprint cache.
#[derive(Debug, Clone)]
pub struct FingerprintCache {
    pool: SqlitePool,
    /// Where this cache actually lives, absolute. Kept so a caller can
    /// report it: the cache sits outside both the data root and the
    /// scan store, so it is the one input a reader cannot infer from
    /// the command line.
    path: PathBuf,
}

impl FingerprintCache {
    /// Open (creating if absent) the cache at `path`, as a plain-SQLite
    /// file.
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("create cache dir {}", dir.display()))?;
        }
        // `filename`, not `from_str`: sqlx's URL parser rejects query
        // parameters it does not know, while the filename field reaches
        // `sqlite3_open_v2` verbatim — but only while sqlx has no URI
        // parameters of its own to add, so `immutable` and `vfs` must
        // stay unset here. See `datalib_progress::bus`.
        let opts = SqliteConnectOptions::new()
            .filename(connect_string(path))
            .create_if_missing(true)
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Wal)
            // A cache. A torn row after a power cut costs one rehash.
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .with_context(|| format!("open fingerprint cache {}", path.display()))?;
        sqlx::query(SCHEMA)
            .execute(&pool)
            .await
            .context("create fingerprints table")?;
        // Absolute, and resolved after creation so the file exists to
        // canonicalize. A relative `--cache-db fp.sqlite` otherwise
        // reports "fp.sqlite", which does not say where.
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        Ok(Self { pool, path })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Where this cache lives, absolute.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Every cached entry at or under `root`, keyed root-relative.
    ///
    /// One indexed range scan: the primary key is the absolute path, so
    /// a subtree is a contiguous run.
    pub async fn load_under(&self, root: &Path) -> Result<CachedTree> {
        let root_s = canonical_root(root).display().to_string();
        let prefix = format!("{}/", root_s.trim_end_matches('/'));
        let rows = sqlx::query(
            "SELECT abs_path, kind, blake3, mtime_ns, size, stamp_kind, inode, dev \
             FROM fingerprints WHERE abs_path = ? OR abs_path GLOB ?",
        )
        .bind(&root_s)
        .bind(format!("{}*", glob_escape(&prefix)))
        .fetch_all(&self.pool)
        .await
        .context("load fingerprints")?;

        let mut tree = CachedTree::default();
        for row in &rows {
            let abs: String = row.try_get("abs_path")?;
            let rel = match abs.strip_prefix(&prefix) {
                Some(r) => r.to_string(),
                None if abs == root_s => String::new(),
                None => continue,
            };
            let digest: Vec<u8> = row.try_get("blake3")?;
            let Ok(blake3) = <Blake3>::try_from(digest.as_slice()) else {
                // A row we cannot read is a cache miss, not an error.
                continue;
            };
            let cursor = StampCursor {
                mtime_ns: row.try_get("mtime_ns")?,
                size: row.try_get("size")?,
                stamp_kind: StampKind::from_str_or_rescan(&row.try_get::<String, _>("stamp_kind")?),
                inode: row.try_get("inode")?,
                dev: row.try_get("dev")?,
            };
            let kind = EntryKind::from_str_or_file(&row.try_get::<String, _>("kind")?);
            tree.entries.insert(rel, (kind, blake3, cursor));
        }
        tree.index_children();
        Ok(tree)
    }

    /// Upsert a batch of observations.
    pub async fn store(&self, batch: &[Fingerprint]) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.context("begin cache tx")?;
        for fp in batch {
            sqlx::query(
                "INSERT INTO fingerprints
                     (abs_path, kind, blake3, mtime_ns, size, stamp_kind, inode, dev)
                 VALUES (?,?,?,?,?,?,?,?)
                 ON CONFLICT(abs_path) DO UPDATE SET
                     kind=excluded.kind, blake3=excluded.blake3,
                     mtime_ns=excluded.mtime_ns, size=excluded.size,
                     stamp_kind=excluded.stamp_kind,
                     inode=excluded.inode, dev=excluded.dev",
            )
            .bind(&fp.abs_path)
            .bind(fp.kind.as_str())
            .bind(&fp.blake3[..])
            .bind(fp.cursor.mtime_ns)
            .bind(fp.cursor.size)
            .bind(fp.cursor.stamp_kind.as_str())
            .bind(fp.cursor.inode)
            .bind(fp.cursor.dev)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("cache {}", fp.abs_path))?;
        }
        tx.commit().await.context("commit cache tx")?;
        Ok(())
    }

    /// Forget specific absolute paths.
    ///
    /// Deliberately dumb: it removes exactly what it is given and
    /// decides nothing. The policy — which entries have earned removal
    /// — belongs to the caller, because only the caller knows the
    /// difference between "this path is gone from the filesystem" and
    /// "my filter did not look at it". Getting that backwards is how
    /// the first version of this let a narrow scan evict a broad one's
    /// work.
    pub async fn forget(&self, abs_paths: &[String]) -> Result<u64> {
        if abs_paths.is_empty() {
            return Ok(0);
        }
        let mut removed = 0u64;
        let mut tx = self.pool.begin().await.context("begin forget tx")?;
        for path in abs_paths {
            let r = sqlx::query("DELETE FROM fingerprints WHERE abs_path = ?")
                .bind(path)
                .execute(&mut *tx)
                .await
                .context("forget fingerprint")?;
            removed += r.rows_affected();
        }
        tx.commit().await.context("commit forget tx")?;
        Ok(removed)
    }

    /// Total rows, for diagnostics.
    pub async fn count(&self) -> Result<i64> {
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM fingerprints")
            .fetch_one(&self.pool)
            .await
            .context("count fingerprints")?;
        Ok(n)
    }
}

/// Escape a literal for use inside a `GLOB` pattern.
///
/// `GLOB` is not `LIKE`: it takes `*`, `?` and `[...]`, and has no
/// escape character — a bracket class is the only way to quote one.
fn glob_escape(literal: &str) -> String {
    let mut out = String::with_capacity(literal.len());
    for ch in literal.chars() {
        match ch {
            '*' | '?' | '[' => {
                out.push('[');
                out.push(ch);
                out.push(']');
            }
            _ => out.push(ch),
        }
    }
    out
}

/// The canonical form of a scan root, as the cache keys it.
///
/// Every entry point normalizes through this rather than trusting the
/// caller, so a relative root, a `..`, a trailing slash and a symlinked
/// route to the same tree all address one set of entries. Absolute
/// keying is the whole basis of "one chain per host": a relative key
/// would put two unrelated trees, scanned as the same relative name
/// from different directories, on top of each other.
///
/// A root that cannot be resolved (it was deleted, say) falls back to
/// the path as given — a lookup then simply finds nothing, which is the
/// right answer.
pub fn canonical_root(root: &Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}

/// Absolute path for a root-relative entry, as the cache keys it.
///
/// `root` is expected to be canonical already — the walker canonicalizes
/// once and reuses it per entry, rather than paying a `canonicalize`
/// syscall per row.
pub fn abs_key(root: &Path, rel: &str) -> String {
    if rel.is_empty() {
        root.display().to_string()
    } else {
        format!(
            "{}/{}",
            root.display().to_string().trim_end_matches('/'),
            rel
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cursor(mtime: i64, size: i64, inode: Option<i64>) -> StampCursor {
        StampCursor {
            mtime_ns: mtime,
            size,
            stamp_kind: if inode.is_some() {
                StampKind::Inode
            } else {
                StampKind::NoStamp
            },
            inode,
            dev: inode.map(|_| 42),
        }
    }

    fn fp(root: &Path, rel: &str, kind: EntryKind, byte: u8) -> Fingerprint {
        Fingerprint {
            abs_path: abs_key(root, rel),
            kind,
            blake3: [byte; 32],
            cursor: cursor(1_000 + i64::from(byte), 7, Some(i64::from(byte))),
        }
    }

    /// The whole point of the file: it must not be a doltlite store.
    #[tokio::test]
    async fn the_cache_is_a_plain_sqlite_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("fingerprints.sqlite");
        let cache = FingerprintCache::open(&path).await.unwrap();
        cache
            .store(&[fp(Path::new("/r"), "a.txt", EntryKind::File, 1)])
            .await
            .unwrap();
        cache.pool().close().await;

        let magic = std::fs::read(&path).unwrap();
        assert_eq!(
            &magic[..15],
            b"SQLite format 3",
            "the cache was created as a doltlite store, so it is paying the \
             per-commit cost this cache exists to avoid"
        );
        assert!(
            !path.with_extension("sqlite-lock").exists(),
            "a doltlite `.-lock` sidecar appeared beside the cache"
        );
    }

    #[tokio::test]
    async fn entries_round_trip_keyed_root_relative() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Path::new("/scan/root");
        let cache = FingerprintCache::open(&tmp.path().join("c.sqlite"))
            .await
            .unwrap();
        cache
            .store(&[
                fp(root, "", EntryKind::Dir, 9),
                fp(root, "docs", EntryKind::Dir, 8),
                fp(root, "docs/a.txt", EntryKind::File, 1),
                fp(root, "b.txt", EntryKind::File, 2),
            ])
            .await
            .unwrap();

        let tree = cache.load_under(root).await.unwrap();
        assert_eq!(tree.len(), 4);
        assert_eq!(tree.blake3("docs/a.txt"), Some([1u8; 32]));
        assert_eq!(tree.kind("docs"), Some(EntryKind::Dir));
        assert_eq!(tree.cursor("b.txt").unwrap().inode, Some(2));
        // The root itself is the empty key.
        assert_eq!(tree.kind(""), Some(EntryKind::Dir));
    }

    #[tokio::test]
    async fn children_are_derived_from_the_key_set() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Path::new("/scan/root");
        let cache = FingerprintCache::open(&tmp.path().join("c.sqlite"))
            .await
            .unwrap();
        cache
            .store(&[
                fp(root, "docs", EntryKind::Dir, 8),
                fp(root, "docs/b.txt", EntryKind::File, 2),
                fp(root, "docs/a.txt", EntryKind::File, 1),
                fp(root, "top.txt", EntryKind::File, 3),
            ])
            .await
            .unwrap();
        let tree = cache.load_under(root).await.unwrap();
        assert_eq!(
            tree.children(""),
            Some(&vec!["docs".to_string(), "top.txt".to_string()]),
            "the root's children are keyed by the empty string, and sorted"
        );
        assert_eq!(
            tree.children("docs"),
            Some(&vec!["docs/a.txt".to_string(), "docs/b.txt".to_string()])
        );
        assert_eq!(tree.children("top.txt"), None, "a file has no children");
    }

    /// Absolute keys are what make one host one chain — a sibling root
    /// must not leak into this one's view.
    #[tokio::test]
    async fn a_sibling_root_is_not_visible() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = FingerprintCache::open(&tmp.path().join("c.sqlite"))
            .await
            .unwrap();
        cache
            .store(&[
                fp(Path::new("/scan/alpha"), "x.txt", EntryKind::File, 1),
                fp(Path::new("/scan/beta"), "y.txt", EntryKind::File, 2),
                // The classic prefix trap: `/scan/alpha2` shares a
                // string prefix with `/scan/alpha` but is not under it.
                fp(Path::new("/scan/alpha2"), "z.txt", EntryKind::File, 3),
            ])
            .await
            .unwrap();

        let tree = cache.load_under(Path::new("/scan/alpha")).await.unwrap();
        assert_eq!(tree.len(), 1);
        assert_eq!(tree.blake3("x.txt"), Some([1u8; 32]));
        assert!(tree.blake3("y.txt").is_none());
        assert!(
            tree.blake3("z.txt").is_none(),
            "`/scan/alpha2` leaked into `/scan/alpha`'s view"
        );
    }

    /// Two roots that overlap share entries rather than duplicating
    /// them — the reason for keying absolutely.
    #[tokio::test]
    async fn a_nested_root_sees_the_outer_scan_s_work() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = FingerprintCache::open(&tmp.path().join("c.sqlite"))
            .await
            .unwrap();
        cache
            .store(&[fp(
                Path::new("/scan/root"),
                "docs/a.txt",
                EntryKind::File,
                5,
            )])
            .await
            .unwrap();

        let inner = cache
            .load_under(Path::new("/scan/root/docs"))
            .await
            .unwrap();
        assert_eq!(
            inner.blake3("a.txt"),
            Some([5u8; 32]),
            "scanning a subdirectory should reuse the outer scan's hashes"
        );
    }

    /// The mirror of the case above, and the one that pays off most:
    /// a scan of a parent must reuse a nested scan's hashes for the
    /// subtree they share, hashing only what is genuinely new to it.
    ///
    /// This is what per-host keying buys over Unison's per-replica-pair
    /// `fpcache`, where scanning `/a` and `/a/b/c` are unrelated jobs
    /// that each hash the overlap from scratch.
    #[tokio::test]
    async fn an_outer_root_reuses_a_nested_scan_s_work() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = FingerprintCache::open(&tmp.path().join("c.sqlite"))
            .await
            .unwrap();
        // A scan of the inner directory happened first.
        cache
            .store(&[
                fp(Path::new("/a/b/c"), "", EntryKind::Dir, 9),
                fp(Path::new("/a/b/c"), "deep.bin", EntryKind::File, 5),
            ])
            .await
            .unwrap();

        // Now the parent is scanned. It should see the inner entries,
        // addressed relative to *its* root.
        let outer = cache.load_under(Path::new("/a")).await.unwrap();
        assert_eq!(
            outer.blake3("b/c/deep.bin"),
            Some([5u8; 32]),
            "the parent scan did not reuse the nested scan's hashes"
        );
        assert_eq!(outer.kind("b/c"), Some(EntryKind::Dir));
    }

    #[tokio::test]
    async fn storing_the_same_path_twice_updates_it() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Path::new("/r");
        let cache = FingerprintCache::open(&tmp.path().join("c.sqlite"))
            .await
            .unwrap();
        cache
            .store(&[fp(root, "a.txt", EntryKind::File, 1)])
            .await
            .unwrap();
        cache
            .store(&[fp(root, "a.txt", EntryKind::File, 2)])
            .await
            .unwrap();
        assert_eq!(cache.count().await.unwrap(), 1);
        let tree = cache.load_under(root).await.unwrap();
        assert_eq!(tree.blake3("a.txt"), Some([2u8; 32]));
    }

    /// The cache is grow-only, and that is the point: a narrower scan
    /// must not evict a broader one's work.
    ///
    /// Two consumers share this cache and disagree about what is
    /// interesting — `fsindex` honours an `ignore` cascade, `pdf` only
    /// wants PDFs. A "delete what this scan did not see" pass would let
    /// whichever scan is narrowest destroy the rest. Measured before it
    /// was removed: an `fsindex` scan ignoring `*.tmp` evicted 200
    /// entries a full scan had just cached, so the next full scan had
    /// to rehash them.
    #[tokio::test]
    async fn a_narrower_scan_does_not_evict_a_broader_one() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Path::new("/r");
        let cache = FingerprintCache::open(&tmp.path().join("c.sqlite"))
            .await
            .unwrap();
        cache
            .store(&[
                fp(root, "doc.pdf", EntryKind::File, 1),
                fp(root, "scratch.tmp", EntryKind::File, 2),
            ])
            .await
            .unwrap();
        // A narrower consumer stores only what it cares about.
        cache
            .store(&[fp(root, "doc.pdf", EntryKind::File, 1)])
            .await
            .unwrap();

        let tree = cache.load_under(root).await.unwrap();
        assert_eq!(
            tree.blake3("scratch.tmp"),
            Some([2u8; 32]),
            "an entry the narrower scan had no opinion about was evicted"
        );
        assert_eq!(cache.count().await.unwrap(), 2);
    }

    #[tokio::test]
    async fn forget_removes_only_what_it_is_given() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Path::new("/r");
        let cache = FingerprintCache::open(&tmp.path().join("c.sqlite"))
            .await
            .unwrap();
        cache
            .store(&[
                fp(root, "a.bin", EntryKind::File, 1),
                fp(root, "b.bin", EntryKind::File, 2),
            ])
            .await
            .unwrap();

        let removed = cache.forget(&[abs_key(root, "a.bin")]).await.unwrap();
        assert_eq!(removed, 1);
        let tree = cache.load_under(root).await.unwrap();
        assert!(tree.blake3("a.bin").is_none());
        assert_eq!(tree.blake3("b.bin"), Some([2u8; 32]));

        // Forgetting something absent is not an error.
        assert_eq!(cache.forget(&[abs_key(root, "never")]).await.unwrap(), 0);
        assert_eq!(cache.forget(&[]).await.unwrap(), 0);
    }

    /// A path containing GLOB metacharacters must not become a pattern.
    #[tokio::test]
    async fn glob_metacharacters_in_a_root_are_literal() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = FingerprintCache::open(&tmp.path().join("c.sqlite"))
            .await
            .unwrap();
        cache
            .store(&[
                fp(Path::new("/od[d]"), "x.txt", EntryKind::File, 1),
                fp(Path::new("/odd"), "y.txt", EntryKind::File, 2),
            ])
            .await
            .unwrap();
        let tree = cache.load_under(Path::new("/od[d]")).await.unwrap();
        assert_eq!(tree.blake3("x.txt"), Some([1u8; 32]));
        assert!(
            tree.blake3("y.txt").is_none(),
            "`[d]` was treated as a character class"
        );
    }

    /// A relative root must never become a relative key: two unrelated
    /// trees scanned as the same relative name from different
    /// directories would land on top of each other. Found by running
    /// the real binary with `--root sub` from two working directories.
    #[tokio::test]
    async fn a_relative_root_is_keyed_absolutely() {
        let tmp = tempfile::tempdir().unwrap();
        let tree = tmp.path().join("tree");
        std::fs::create_dir_all(&tree).unwrap();
        let cache = FingerprintCache::open(&tmp.path().join("c.sqlite"))
            .await
            .unwrap();

        let canonical = canonical_root(&tree);
        assert!(canonical.is_absolute());
        cache
            .store(&[fp(&canonical, "x.bin", EntryKind::File, 7)])
            .await
            .unwrap();

        // A non-canonical spelling of the same root finds it.
        let via_dotdot = tree.join("..").join("tree");
        assert_eq!(
            cache.load_under(&via_dotdot).await.unwrap().blake3("x.bin"),
            Some([7u8; 32]),
            "a non-canonical root missed its own entries"
        );
    }

    #[test]
    fn an_unresolvable_root_falls_back_to_the_path_as_given() {
        // A deleted root simply finds nothing; it must not panic.
        let p = Path::new("/definitely/not/here/at/all");
        assert_eq!(canonical_root(p), p.to_path_buf());
    }

    #[test]
    fn glob_escaping_quotes_every_metacharacter() {
        assert_eq!(glob_escape("plain/path/"), "plain/path/");
        assert_eq!(glob_escape("a*b"), "a[*]b");
        assert_eq!(glob_escape("a?b"), "a[?]b");
        assert_eq!(glob_escape("a[b"), "a[[]b");
    }

    #[test]
    fn the_default_path_is_a_cache_dir_not_a_data_root() {
        // Host state must not land somewhere that gets synced or copied.
        temp_env_var("DATALIB_CACHE_DIR", Some("/tmp/explicit"), || {
            assert_eq!(
                default_cache_path().unwrap(),
                PathBuf::from("/tmp/explicit/fingerprints.sqlite")
            );
        });
        temp_env_var("DATALIB_CACHE_DIR", None, || {
            temp_env_var("XDG_CACHE_HOME", Some("/tmp/xdg"), || {
                assert_eq!(
                    default_cache_path().unwrap(),
                    PathBuf::from("/tmp/xdg/datalib/fingerprints.sqlite")
                );
            });
        });
    }

    /// `std::env::set_var` is unsafe from Rust 2024 and racy under a
    /// threaded test runner; these two env tests are the only users, and
    /// they run in one thread each.
    fn temp_env_var(key: &str, value: Option<&str>, f: impl FnOnce()) {
        let prev = std::env::var_os(key);
        match value {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
        f();
        match prev {
            Some(v) => std::env::set_var(key, v),
            None => std::env::remove_var(key),
        }
    }
}
