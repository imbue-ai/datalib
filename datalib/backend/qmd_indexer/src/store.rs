//! qmd's `index.sqlite`, opened from Rust: one group's collection written
//! the way `reindexCollection` (`third-party/qmd/src/store.ts`) writes it,
//! and the per-collection embedding gauge qmd's own `embed` decides by.
//!
//! What is here is a port of qmd's write path, not a schema of our own.
//! `documents.path` is the literal path relative to the collection root,
//! `hash` is SHA-256 of the file's UTF-8 text, `title` is the first
//! heading, and the FTS rows come from the triggers qmd declares on
//! `documents`. A later `qmd update` reports every row we write as
//! `unchanged` — the integration test pins that, because it is the whole
//! basis for indexing one source without qmd's all-collections `update`.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

/// The busy timeout qmd itself opens the store with (`src/db.ts`), so a
/// writer here queues behind an embed the way a second qmd would.
const BUSY_TIMEOUT: Duration = Duration::from_secs(120);

/// Documents written per transaction. Small enough that a reader sees
/// progress and a Ctrl-C loses little; large enough that the per-commit
/// cost does not dominate.
const BATCH: usize = 200;

pub async fn open_rw(path: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .busy_timeout(BUSY_TIMEOUT);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .with_context(|| format!("open qmd index {}", path.display()))
}

pub async fn open_ro(path: &Path) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(false)
        .read_only(true)
        .busy_timeout(BUSY_TIMEOUT);
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(opts)
        .await
        .with_context(|| format!("open qmd index {} read-only", path.display()))
}

/// What one pass over a group's tree did to its collection. The counts
/// are qmd's own (`ReindexResult`), so a log line reads the same either
/// way.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GroupIndexSummary {
    pub indexed: u64,
    pub updated: u64,
    pub unchanged: u64,
    pub removed: u64,
    /// Files with nothing but whitespace in them, which qmd does not
    /// index either.
    pub skipped_empty: u64,
    pub orphaned_content: u64,
    /// Active documents in the collection after the pass.
    pub documents: u64,
    /// Content version of the collection: a digest over its active
    /// `(path, hash)` pairs and the qmd pin. Stable across runs that
    /// change nothing; moves when a document does, or when a qmd bump
    /// means the same rows would be read differently.
    pub version: String,
}

pub trait IndexProgress {
    fn total(&self, _files: u64) {}
    fn done(&self, _files: u64) {}
}

pub struct NoIndexProgress;
impl IndexProgress for NoIndexProgress {}

struct Existing {
    hash: String,
    title: String,
    active: bool,
}

/// Bring `collection`'s rows in line with the `*.md` files under
/// `tree`, whose paths are stored relative to `collection_root` (the
/// data root — see `mask_for_group` for why).
pub async fn index_group(
    pool: &SqlitePool,
    collection: &str,
    collection_root: &Path,
    tree: &Path,
    qmd_version: &str,
    progress: &dyn IndexProgress,
) -> Result<GroupIndexSummary> {
    let files = markdown_files(tree);
    progress.total(files.len() as u64);
    let now = iso_now();

    let mut existing: BTreeMap<String, Existing> = BTreeMap::new();
    let rows = sqlx::query("SELECT path, hash, title, active FROM documents WHERE collection = ?")
        .bind(collection)
        .fetch_all(pool)
        .await
        .context("read the collection's documents")?;
    for r in rows {
        existing.insert(
            r.try_get::<String, _>("path")?,
            Existing {
                hash: r.try_get("hash")?,
                title: r.try_get("title")?,
                active: r.try_get::<i64, _>("active")? != 0,
            },
        );
    }

    let mut summary = GroupIndexSummary::default();
    let mut seen: Vec<String> = Vec::with_capacity(files.len());
    let mut done: u64 = 0;
    for chunk in files.chunks(BATCH) {
        let mut conn = pool.acquire().await?;
        // IMMEDIATE, not DEFERRED: a deferred transaction that reads and
        // then writes under WAL can fail with SQLITE_BUSY at the write
        // instead of waiting out `busy_timeout`.
        sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
        for file in chunk {
            done += 1;
            let rel = relative_posix(collection_root, file)?;
            let bytes = std::fs::read(file).with_context(|| format!("read {}", file.display()))?;
            let text = String::from_utf8_lossy(&bytes);
            if text.trim().is_empty() {
                summary.skipped_empty += 1;
                continue;
            }
            let hash = sha256_hex(text.as_bytes());
            let title = extract_title(&text, &rel);
            let meta = std::fs::metadata(file).ok();
            let mtime = meta
                .as_ref()
                .and_then(|m| m.modified().ok())
                .map(iso_from_system_time)
                .unwrap_or_else(|| now.clone());
            seen.push(rel.clone());

            sqlx::query("INSERT OR IGNORE INTO content (hash, doc, created_at) VALUES (?, ?, ?)")
                .bind(&hash)
                .bind(text.as_ref())
                .bind(&now)
                .execute(&mut *conn)
                .await?;

            match existing.get(&rel) {
                Some(e) if e.active && e.hash == hash && e.title == title => {
                    summary.unchanged += 1;
                }
                Some(e) if e.active && e.hash == hash => {
                    // Same bytes, different heading: qmd stamps `now` here
                    // rather than the file's mtime.
                    sqlx::query(
                        "UPDATE documents SET title = ?, modified_at = ? \
                         WHERE collection = ? AND path = ?",
                    )
                    .bind(&title)
                    .bind(&now)
                    .bind(collection)
                    .bind(&rel)
                    .execute(&mut *conn)
                    .await?;
                    summary.updated += 1;
                }
                Some(e) if e.active => {
                    sqlx::query(
                        "UPDATE documents SET title = ?, hash = ?, modified_at = ? \
                         WHERE collection = ? AND path = ?",
                    )
                    .bind(&title)
                    .bind(&hash)
                    .bind(&mtime)
                    .bind(collection)
                    .bind(&rel)
                    .execute(&mut *conn)
                    .await?;
                    summary.updated += 1;
                }
                Some(_) => {
                    // A path that was deactivated and is back. qmd reaches
                    // this through its upsert's `active = 1`; the UNIQUE
                    // key means the row is reused rather than duplicated.
                    sqlx::query(
                        "UPDATE documents SET title = ?, hash = ?, modified_at = ?, active = 1 \
                         WHERE collection = ? AND path = ?",
                    )
                    .bind(&title)
                    .bind(&hash)
                    .bind(&mtime)
                    .bind(collection)
                    .bind(&rel)
                    .execute(&mut *conn)
                    .await?;
                    summary.indexed += 1;
                }
                None => {
                    let birth = meta
                        .as_ref()
                        .and_then(|m| m.created().ok())
                        .map(iso_from_system_time)
                        .unwrap_or_else(|| now.clone());
                    sqlx::query(
                        "INSERT INTO documents \
                         (collection, path, title, hash, created_at, modified_at, active) \
                         VALUES (?, ?, ?, ?, ?, ?, 1)",
                    )
                    .bind(collection)
                    .bind(&rel)
                    .bind(&title)
                    .bind(&hash)
                    .bind(&birth)
                    .bind(&mtime)
                    .execute(&mut *conn)
                    .await?;
                    summary.indexed += 1;
                }
            }
        }
        sqlx::query("COMMIT").execute(&mut *conn).await?;
        progress.done(done);
    }

    // Everything active that this walk did not see is gone from disk.
    // Deactivated rather than deleted, as qmd does; its content row
    // survives too (`cleanupOrphanedContent` keeps a hash any document
    // row still names, active or not).
    let mut conn = pool.acquire().await?;
    sqlx::query("BEGIN IMMEDIATE").execute(&mut *conn).await?;
    let seen_set: std::collections::BTreeSet<&str> = seen.iter().map(String::as_str).collect();
    for (path, e) in &existing {
        if e.active && !seen_set.contains(path.as_str()) {
            sqlx::query(
                "UPDATE documents SET active = 0 WHERE collection = ? AND path = ? AND active = 1",
            )
            .bind(collection)
            .bind(path)
            .execute(&mut *conn)
            .await?;
            summary.removed += 1;
        }
    }
    let cleaned =
        sqlx::query("DELETE FROM content WHERE hash NOT IN (SELECT DISTINCT hash FROM documents)")
            .execute(&mut *conn)
            .await?;
    summary.orphaned_content = cleaned.rows_affected();
    sqlx::query("COMMIT").execute(&mut *conn).await?;
    drop(conn);

    let (documents, version) = collection_version(pool, collection, qmd_version).await?;
    summary.documents = documents;
    summary.version = version;
    Ok(summary)
}

/// The collection's content version and its active document count.
pub async fn collection_version(
    pool: &SqlitePool,
    collection: &str,
    qmd_version: &str,
) -> Result<(u64, String)> {
    let rows = sqlx::query(
        "SELECT path, hash FROM documents WHERE collection = ? AND active = 1 ORDER BY path",
    )
    .bind(collection)
    .fetch_all(pool)
    .await?;
    let mut h = Sha256::new();
    h.update(format!("qmd={qmd_version}\n").as_bytes());
    for r in &rows {
        h.update(r.try_get::<String, _>("path")?.as_bytes());
        h.update(b"\0");
        h.update(r.try_get::<String, _>("hash")?.as_bytes());
        h.update(b"\n");
    }
    Ok((rows.len() as u64, hex(&h.finalize())))
}

/// How much of one collection semantic search can reach, by qmd's own
/// definition (`getHashesNeedingEmbedding` in `store.ts`): a document is
/// pending until every chunk of its hash has a vector under the model
/// and fingerprint qmd is currently writing.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct EmbedGauge {
    pub active: u64,
    pub pending: u64,
    pub chunks: u64,
}

impl EmbedGauge {
    pub fn embedded(&self) -> u64 {
        self.active.saturating_sub(self.pending)
    }
}

/// Which `(model, embed_fingerprint)` qmd is writing is only knowable
/// from the rows it wrote: the newest one. Before the first vector ever
/// lands there is nothing to read, and a document with no vectors at
/// all is pending under any fingerprint — so that is what the cold
/// answer counts. It cannot see a fingerprint change until the first
/// new-fingerprint row lands, at which point the gauge jumps to the
/// right number; `qmd embed` itself is never misled, because it
/// computes the fingerprint rather than reading it.
pub async fn embed_gauge(pool: &SqlitePool, collection: &str) -> Result<EmbedGauge> {
    let active: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM documents WHERE collection = ? AND active = 1")
            .bind(collection)
            .fetch_one(pool)
            .await?;
    let current = sqlx::query(
        "SELECT model, embed_fingerprint FROM content_vectors ORDER BY rowid DESC LIMIT 1",
    )
    .fetch_optional(pool)
    .await?;
    let pending: i64 = match current {
        Some(r) => {
            let model: String = r.try_get("model")?;
            let fingerprint: String = r.try_get("embed_fingerprint")?;
            sqlx::query_scalar(
                "SELECT COUNT(DISTINCT d.hash) FROM documents d \
                 LEFT JOIN (SELECT hash, COUNT(*) AS chunk_count, MAX(total_chunks) AS expected_chunks \
                            FROM content_vectors WHERE model = ? AND embed_fingerprint = ? \
                            GROUP BY hash) v ON d.hash = v.hash \
                 WHERE d.active = 1 AND d.collection = ? \
                   AND (v.hash IS NULL OR v.chunk_count < v.expected_chunks)",
            )
            .bind(&model)
            .bind(&fingerprint)
            .bind(collection)
            .fetch_one(pool)
            .await?
        }
        None => {
            sqlx::query_scalar(
                "SELECT COUNT(DISTINCT d.hash) FROM documents d \
                 WHERE d.active = 1 AND d.collection = ? \
                   AND NOT EXISTS (SELECT 1 FROM content_vectors v WHERE v.hash = d.hash)",
            )
            .bind(collection)
            .fetch_one(pool)
            .await?
        }
    };
    let chunks: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM content_vectors v \
         WHERE v.hash IN (SELECT hash FROM documents WHERE collection = ? AND active = 1)",
    )
    .bind(collection)
    .fetch_one(pool)
    .await?;
    Ok(EmbedGauge {
        active: active as u64,
        pending: pending as u64,
        chunks: chunks as u64,
    })
}

/// Every `*.md` under `tree`, sorted, skipping any path component that
/// starts with `.` — qmd's glob runs with `dot: false`.
fn markdown_files(tree: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    if !tree.is_dir() {
        return out;
    }
    let walker = walkdir::WalkDir::new(tree)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| {
            e.depth() == 0 || !e.file_name().to_str().is_some_and(|n| n.starts_with('.'))
        });
    for entry in walker.flatten() {
        if entry.file_type().is_file() && entry.path().extension().is_some_and(|x| x == "md") {
            out.push(entry.into_path());
        }
    }
    out
}

fn relative_posix(root: &Path, file: &Path) -> Result<String> {
    let rel = file
        .strip_prefix(root)
        .with_context(|| format!("{} is not under {}", file.display(), root.display()))?;
    Ok(rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy())
        .collect::<Vec<_>>()
        .join("/"))
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    hex(&h.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// qmd's `extractTitle` for `.md`: the first `#` or `##` heading, with
/// its "Notes" special case, else the file's stem. The regex it uses is
/// `/^##?\s+(.+)$/m`, where `\s+` may run across line breaks.
pub fn extract_title(text: &str, rel_path: &str) -> String {
    if let Some(t) = first_heading(text, false) {
        if t == "📝 Notes" || t == "Notes" {
            if let Some(next) = first_heading(text, true) {
                return next;
            }
        }
        return t;
    }
    let name = rel_path.rsplit('/').next().unwrap_or(rel_path);
    match name.rfind('.') {
        Some(i) if i > 0 => name[..i].to_string(),
        _ => name.to_string(),
    }
}

fn first_heading(text: &str, level_two_only: bool) -> Option<String> {
    let bytes = text.as_bytes();
    let mut at_line_start = true;
    let mut i = 0;
    while i < bytes.len() {
        if at_line_start && bytes[i] == b'#' {
            let mut j = i + 1;
            let mut hashes = 1;
            if j < bytes.len() && bytes[j] == b'#' {
                j += 1;
                hashes = 2;
            }
            let wanted = !level_two_only || hashes == 2;
            if wanted && j < bytes.len() && text[j..].starts_with(char::is_whitespace) {
                let rest = text[j..].trim_start();
                let line_end = rest.find(['\n', '\r']).unwrap_or(rest.len());
                let title = rest[..line_end].trim();
                if !title.is_empty() {
                    return Some(title.to_string());
                }
            }
        }
        at_line_start = bytes[i] == b'\n' || bytes[i] == b'\r';
        i += 1;
    }
    None
}

fn iso_now() -> String {
    iso_from_system_time(SystemTime::now())
}

/// `Date.prototype.toISOString` — UTC, millisecond precision, `Z`.
pub fn iso_from_system_time(t: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(t)
        .format("%Y-%m-%dT%H:%M:%S%.3fZ")
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn title_is_the_first_heading_of_either_level() {
        assert_eq!(
            extract_title("intro\n## Second\n# First\n", "a/b.md"),
            "Second"
        );
        assert_eq!(extract_title("# Top  \nbody", "a/b.md"), "Top");
        assert_eq!(extract_title("### too deep\n", "a/b.md"), "b");
        assert_eq!(extract_title("#nospace\n", "x.md"), "x");
    }

    /// qmd's `\s+` runs across line breaks, so a bare `#` line takes the
    /// next non-empty line as its title.
    #[test]
    fn heading_whitespace_may_span_lines() {
        assert_eq!(extract_title("#\n\nLater\n", "x.md"), "Later");
    }

    #[test]
    fn notes_heading_defers_to_the_first_h2() {
        assert_eq!(
            extract_title("# Notes\n\n## Real title\n", "x.md"),
            "Real title"
        );
        assert_eq!(extract_title("# 📝 Notes\n", "dir/notes.md"), "📝 Notes");
    }

    #[test]
    fn fallback_title_is_the_stem_of_the_last_segment() {
        assert_eq!(
            extract_title("no headings", "g/render_markdown/c/2026-01.md"),
            "2026-01"
        );
        assert_eq!(extract_title("", ".hidden"), ".hidden");
    }

    #[test]
    fn iso_matches_javascript() {
        let t = SystemTime::UNIX_EPOCH + Duration::from_millis(1_757_916_000_123);
        assert_eq!(iso_from_system_time(t), "2025-09-15T06:00:00.123Z");
        assert_eq!(
            iso_from_system_time(SystemTime::UNIX_EPOCH),
            "1970-01-01T00:00:00.000Z"
        );
    }

    #[test]
    fn markdown_walk_skips_dot_components_and_sorts() {
        let td = tempfile::tempdir().unwrap();
        let t = td.path();
        std::fs::create_dir_all(t.join("b/.cache")).unwrap();
        std::fs::create_dir_all(t.join("a")).unwrap();
        std::fs::write(t.join("b/x.md"), "x").unwrap();
        std::fs::write(t.join("b/.cache/y.md"), "y").unwrap();
        std::fs::write(t.join("a/.dot.md"), "z").unwrap();
        std::fs::write(t.join("a/z.md"), "z").unwrap();
        std::fs::write(t.join("a/z.txt"), "z").unwrap();
        let got: Vec<String> = markdown_files(t)
            .iter()
            .map(|p| relative_posix(t, p).unwrap())
            .collect();
        assert_eq!(got, vec!["a/z.md", "b/x.md"]);
    }
}
