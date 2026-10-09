//! A host-local cache of "what did this path look like, and what was its
//! hash" — the fast-rescan cursor, kept out of versioned storage.
//!
//! Host state, not versioned state: an inode number means nothing on another
//! machine, and the live filesystem has no history to branch. Plain SQLite,
//! keyed by absolute path so overlapping roots share work. The crate README
//! has the measurements.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
use sqlx::{Row, SqlitePool};

use crate::fswalk::{Blake3, StampCursor, StampKind};

/// What kind of thing a cached entry describes.
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

    pub fn cursor(&self, rel: &str) -> Option<&StampCursor> {
        self.entries.get(rel).map(|(_, _, c)| c)
    }

    pub fn blake3(&self, rel: &str) -> Option<Blake3> {
        self.entries.get(rel).map(|(_, h, _)| *h)
    }

    pub fn kind(&self, rel: &str) -> Option<EntryKind> {
        self.entries.get(rel).map(|(k, _, _)| *k)
    }

    pub fn children(&self, rel: &str) -> Option<&Vec<String>> {
        self.children.get(rel)
    }

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
    ctime_ns    INTEGER,
    stamp_kind  TEXT NOT NULL,
    inode       INTEGER,
    dev         INTEGER
)";

/// The shape of [`SCHEMA`], kept in the file's `user_version`. A cache in
/// any other shape is dropped and refilled: it is a cache, so that costs
/// one rehash of whatever is scanned next, and nothing else.
const SCHEMA_VERSION: i64 = 2;

pub fn default_cache_path() -> Result<PathBuf> {
    cache_path_from_env(|key| std::env::var_os(key), cfg!(target_os = "macos"))
}

fn cache_path_from_env(var: impl Fn(&str) -> Option<OsString>, macos: bool) -> Result<PathBuf> {
    if let Some(dir) = var("DATALIB_CACHE_DIR") {
        return Ok(PathBuf::from(dir).join("fingerprints.sqlite"));
    }
    // Bazel sets TEST_TMPDIR for every test and the processes it spawns.
    // What follows is the developer's real cache, where every sandbox path
    // a test scans would stay as a dead row.
    if var("TEST_TMPDIR").is_some() {
        bail!(
            "DATALIB_CACHE_DIR is not set under a bazel test (TEST_TMPDIR is): \
             point it at a directory under $TEST_TMPDIR rather than writing \
             sandbox paths into this host's fingerprint cache"
        );
    }
    if let Some(dir) = var("XDG_CACHE_HOME") {
        return Ok(PathBuf::from(dir)
            .join("datalib")
            .join("fingerprints.sqlite"));
    }
    let home = var("HOME")
        .map(PathBuf::from)
        .context("neither DATALIB_CACHE_DIR, XDG_CACHE_HOME nor HOME is set")?;
    let base = if macos {
        home.join("Library").join("Caches")
    } else {
        home.join(".cache")
    };
    Ok(base.join("datalib").join("fingerprints.sqlite"))
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
    pub async fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)
                .with_context(|| format!("create cache dir {}", dir.display()))?;
        }
        let opts = SqliteConnectOptions::new()
            .filename(datalib_runtime::plain_sqlite::uri(path))
            .create_if_missing(true)
            // Doltlite's plain-SQLite engine answers `wal` and stays in
            // rollback-journal mode; ask for the mode the file is in.
            .journal_mode(sqlx::sqlite::SqliteJournalMode::Delete)
            // A cache. A torn row after a power cut costs one rehash.
            .synchronous(sqlx::sqlite::SqliteSynchronous::Normal);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(opts)
            .await
            .with_context(|| format!("open fingerprint cache {}", path.display()))?;
        // One write transaction, because steps running side by side open
        // this one file: another's check must not see the old table gone
        // and the new one not yet made, or drop the table it just made.
        let mut tx = pool
            .begin_with("BEGIN IMMEDIATE")
            .await
            .context("lock fingerprint cache to check its shape")?;
        let version: i64 = sqlx::query_scalar("PRAGMA user_version")
            .fetch_one(&mut *tx)
            .await
            .context("read fingerprint cache version")?;
        if version != SCHEMA_VERSION {
            sqlx::query("DROP TABLE IF EXISTS fingerprints")
                .execute(&mut *tx)
                .await
                .context("drop an older fingerprints table")?;
            sqlx::query(SCHEMA)
                .execute(&mut *tx)
                .await
                .context("create fingerprints table")?;
            // Audited: an integer constant, not input.
            sqlx::query(sqlx::AssertSqlSafe(format!(
                "PRAGMA user_version = {SCHEMA_VERSION}"
            )))
            .execute(&mut *tx)
            .await
            .context("stamp fingerprint cache version")?;
        }
        tx.commit()
            .await
            .context("commit fingerprint cache shape")?;
        // Absolute, and resolved after creation so the file exists to
        // canonicalize. A relative `--cache-db fp.sqlite` otherwise
        // reports "fp.sqlite", which does not say where.
        let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        Ok(Self { pool, path })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

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
            "SELECT abs_path, kind, blake3, mtime_ns, size, ctime_ns, stamp_kind, inode, dev \
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
                ctime_ns: row.try_get("ctime_ns")?,
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

    /// Make the cached row for `path` match the file's stat now, keeping
    /// its hash, so the next scan vouches for the file without opening it.
    /// Wrong for anything but a test: it stands in for a file that changed
    /// between a scan and a read of it, which a chmod cannot, since a chmod
    /// moves the change time.
    pub async fn restamp_for_test(&self, path: &Path) -> Result<()> {
        let fresh = crate::fswalk::fresh_stat(
            &std::fs::metadata(path).with_context(|| format!("stat {}", path.display()))?,
        );
        let key = path
            .canonicalize()
            .with_context(|| format!("resolve {}", path.display()))?
            .display()
            .to_string();
        let done = sqlx::query(
            "UPDATE fingerprints SET mtime_ns = ?, size = ?, ctime_ns = ?, inode = ?, dev = ? \
             WHERE abs_path = ?",
        )
        .bind(fresh.mtime_ns)
        .bind(fresh.size)
        .bind(fresh.ctime_ns)
        .bind(fresh.inode)
        .bind(fresh.dev)
        .bind(&key)
        .execute(&self.pool)
        .await
        .context("restamp a fingerprint")?;
        if done.rows_affected() != 1 {
            bail!("no cached fingerprint for {key}");
        }
        Ok(())
    }

    pub async fn store(&self, batch: &[Fingerprint]) -> Result<()> {
        if batch.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.context("begin cache tx")?;
        for fp in batch {
            sqlx::query(
                "INSERT INTO fingerprints
                     (abs_path, kind, blake3, mtime_ns, size, ctime_ns, stamp_kind, inode, dev)
                 VALUES (?,?,?,?,?,?,?,?,?)
                 ON CONFLICT(abs_path) DO UPDATE SET
                     kind=excluded.kind, blake3=excluded.blake3,
                     mtime_ns=excluded.mtime_ns, size=excluded.size,
                     ctime_ns=excluded.ctime_ns,
                     stamp_kind=excluded.stamp_kind,
                     inode=excluded.inode, dev=excluded.dev",
            )
            .bind(&fp.abs_path)
            .bind(fp.kind.as_str())
            .bind(&fp.blake3[..])
            .bind(fp.cursor.mtime_ns)
            .bind(fp.cursor.size)
            .bind(fp.cursor.ctime_ns)
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

    pub fn disk_bytes(&self) -> u64 {
        let mut total = 0u64;
        for suffix in ["", "-wal", "-shm"] {
            let mut p = self.path.clone().into_os_string();
            p.push(suffix);
            if let Ok(md) = std::fs::metadata(PathBuf::from(p)) {
                total += md.len();
            }
        }
        total
    }

    pub async fn checkpoint(&self) {
        let _ = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(&self.pool)
            .await;
    }

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

pub fn canonical_root(root: &Path) -> PathBuf {
    root.canonicalize().unwrap_or_else(|_| root.to_path_buf())
}

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
            ctime_ns: Some(mtime + 1),
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

    /// A cache an older build wrote has no ctime to compare, so its rows
    /// would vouch for files the current check would rehash. It goes.
    #[tokio::test]
    async fn a_cache_in_an_older_shape_is_dropped_not_read() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("c.sqlite");
        let old = SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(
                SqliteConnectOptions::new()
                    .filename(&path)
                    .create_if_missing(true),
            )
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE fingerprints (abs_path TEXT PRIMARY KEY, kind TEXT NOT NULL, \
             blake3 BLOB NOT NULL, mtime_ns INTEGER NOT NULL, size INTEGER NOT NULL, \
             stamp_kind TEXT NOT NULL, inode INTEGER, dev INTEGER)",
        )
        .execute(&old)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO fingerprints VALUES ('/r/a', 'file', zeroblob(32), 1, 1, 'inode', 1, 1)",
        )
        .execute(&old)
        .await
        .unwrap();
        old.close().await;

        let cache = FingerprintCache::open(&path).await.unwrap();
        assert!(cache.load_under(Path::new("/r")).await.unwrap().is_empty());
        cache
            .store(&[fp(Path::new("/r"), "a", EntryKind::File, 1)])
            .await
            .unwrap();
        let tree = cache.load_under(Path::new("/r")).await.unwrap();
        assert_eq!(tree.cursor("a").unwrap().ctime_ns, Some(1_002));
        cache.pool().close().await;
    }

    /// Steps running side by side open one cache file. One open's shape
    /// check dropped the table another had just made, and that one's
    /// first read failed with "no such table".
    #[tokio::test(flavor = "multi_thread", worker_threads = 8)]
    async fn opens_side_by_side_never_see_the_table_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("c.sqlite");
        let opens: Vec<_> = (0..16)
            .map(|_| {
                let path = path.clone();
                tokio::spawn(async move {
                    let cache = FingerprintCache::open(&path).await?;
                    cache.load_under(Path::new("/r")).await?;
                    cache.pool().close().await;
                    anyhow::Ok(())
                })
            })
            .collect();
        for open in opens {
            open.await.unwrap().unwrap();
        }
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

    fn env<'a>(pairs: &'a [(&str, &str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        |key| {
            pairs
                .iter()
                .find(|(k, _)| *k == key)
                .map(|(_, v)| OsString::from(v))
        }
    }

    #[test]
    fn the_default_path_is_a_cache_dir_not_a_data_root() {
        // Host state must not land somewhere that gets synced or copied.
        let resolve = |pairs: &[(&str, &str)], macos| cache_path_from_env(env(pairs), macos);
        assert_eq!(
            resolve(
                &[("DATALIB_CACHE_DIR", "/tmp/explicit"), ("HOME", "/h")],
                true
            )
            .unwrap(),
            PathBuf::from("/tmp/explicit/fingerprints.sqlite")
        );
        assert_eq!(
            resolve(&[("XDG_CACHE_HOME", "/tmp/xdg"), ("HOME", "/h")], true).unwrap(),
            PathBuf::from("/tmp/xdg/datalib/fingerprints.sqlite")
        );
        assert_eq!(
            resolve(&[("HOME", "/h")], true).unwrap(),
            PathBuf::from("/h/Library/Caches/datalib/fingerprints.sqlite")
        );
        assert_eq!(
            resolve(&[("HOME", "/h")], false).unwrap(),
            PathBuf::from("/h/.cache/datalib/fingerprints.sqlite")
        );
    }

    /// Guards the leak that left ~190k dead sandbox rows in a developer's
    /// real cache: a bazel test must name its own cache directory.
    #[test]
    fn a_bazel_test_must_name_its_cache_dir() {
        let under_test = [
            ("TEST_TMPDIR", "/t"),
            ("HOME", "/h"),
            ("XDG_CACHE_HOME", "/x"),
        ];
        let err = cache_path_from_env(env(&under_test), true).unwrap_err();
        assert!(err.to_string().contains("DATALIB_CACHE_DIR"), "{err}");

        let named = [("TEST_TMPDIR", "/t"), ("DATALIB_CACHE_DIR", "/t/cache")];
        assert_eq!(
            cache_path_from_env(env(&named), true).unwrap(),
            PathBuf::from("/t/cache/fingerprints.sqlite")
        );
    }
}
