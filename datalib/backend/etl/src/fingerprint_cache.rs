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
//! One chain per host, not per root. Absolute keys mean overlapping and
//! nested scan roots share entries instead of duplicating them, a root
//! that moves simply misses rather than colliding, and two providers
//! scanning the same tree reuse each other's work.

use std::collections::{BTreeSet, HashMap};
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
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Every cached entry at or under `root`, keyed root-relative.
    ///
    /// One indexed range scan: the primary key is the absolute path, so
    /// a subtree is a contiguous run.
    pub async fn load_under(&self, root: &Path) -> Result<CachedTree> {
        let root_s = root.display().to_string();
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

    /// Drop cached entries under `root` that this scan did not see.
    ///
    /// `kept` holds root-relative paths. Without this the cache grows
    /// forever across deletes, and — worse — a path that is deleted and
    /// later recreated could match a stale cursor.
    pub async fn prune_missing(&self, root: &Path, kept: &BTreeSet<String>) -> Result<u64> {
        let cached = self.load_under(root).await?;
        let root_s = root.display().to_string();
        let prefix = format!("{}/", root_s.trim_end_matches('/'));
        let stale: Vec<String> = cached
            .entries
            .keys()
            .filter(|rel| !kept.contains(*rel))
            .map(|rel| {
                if rel.is_empty() {
                    root_s.clone()
                } else {
                    format!("{prefix}{rel}")
                }
            })
            .collect();
        if stale.is_empty() {
            return Ok(0);
        }
        let mut removed = 0u64;
        let mut tx = self.pool.begin().await.context("begin prune tx")?;
        for path in &stale {
            let r = sqlx::query("DELETE FROM fingerprints WHERE abs_path = ?")
                .bind(path)
                .execute(&mut *tx)
                .await
                .context("prune fingerprint")?;
            removed += r.rows_affected();
        }
        tx.commit().await.context("commit prune tx")?;
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

/// Absolute path for a root-relative entry, as the cache keys it.
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

    #[tokio::test]
    async fn pruning_drops_what_the_scan_did_not_see() {
        let tmp = tempfile::tempdir().unwrap();
        let root = Path::new("/r");
        let cache = FingerprintCache::open(&tmp.path().join("c.sqlite"))
            .await
            .unwrap();
        cache
            .store(&[
                fp(root, "kept.txt", EntryKind::File, 1),
                fp(root, "gone.txt", EntryKind::File, 2),
                fp(root, "also/gone.txt", EntryKind::File, 3),
            ])
            .await
            .unwrap();

        let kept: BTreeSet<String> = ["kept.txt".to_string()].into_iter().collect();
        let removed = cache.prune_missing(root, &kept).await.unwrap();
        assert_eq!(removed, 2);

        let tree = cache.load_under(root).await.unwrap();
        assert_eq!(tree.len(), 1);
        assert!(tree.blake3("kept.txt").is_some());
    }

    #[tokio::test]
    async fn pruning_leaves_other_roots_alone() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = FingerprintCache::open(&tmp.path().join("c.sqlite"))
            .await
            .unwrap();
        cache
            .store(&[
                fp(Path::new("/a"), "x.txt", EntryKind::File, 1),
                fp(Path::new("/b"), "y.txt", EntryKind::File, 2),
            ])
            .await
            .unwrap();
        cache
            .prune_missing(Path::new("/a"), &BTreeSet::new())
            .await
            .unwrap();
        assert_eq!(cache.count().await.unwrap(), 1);
        assert!(cache
            .load_under(Path::new("/b"))
            .await
            .unwrap()
            .blake3("y.txt")
            .is_some());
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
