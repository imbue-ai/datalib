//! Content-addressable blob store + per-bucket attachment bundle.
//!
//! Bytes live once in `cas_objects`, keyed by blake3; each provider declares
//! its own `(owning_id, ref_id, blake3)` edge table. Download fills a bundle,
//! parse loads every document's bundle in one pass, and render consumes an
//! already-loaded bag of bytes. See the crate README.
//!
//! One payload can reach a bundle under two refs with different metadata, so
//! filenames dedupe on the content hash rather than on the derived name.

use std::collections::btree_map::Entry;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::hash::Hash;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow, SqliteSynchronous};
use sqlx::{Row, SqlitePool};

// Schema

/// Sole table in the per-source blobs database. Pure content-addressed
/// storage: bytes keyed by their blake3, nothing source-specific — and
/// no stamp, since when a row arrived is the date of the commit that
/// brought it.
pub const CAS_OBJECTS_DDL: &str = "CREATE TABLE IF NOT EXISTS cas_objects (
    blake3            TEXT PRIMARY KEY,
    byte_len          INTEGER NOT NULL,
    content_type      TEXT NULL,
    bytes             BLOB NOT NULL,
    CHECK (length(blake3) = 64)
)";

// Path helpers

/// Given the entity db path (e.g. `/x/raw/slack/entities.doltlite_db`),
/// return the sibling CAS path `/x/raw/slack/blobs.sqlite`. Both files
/// live inside the per-source directory; the filename comes from
/// [`crate::raw_layout::BLOBS_DB`].
pub fn cas_path_for(entity_db_path: &Path) -> PathBuf {
    let parent = entity_db_path.parent().unwrap_or_else(|| Path::new("."));
    crate::raw_layout::blobs_db(parent)
}

// CAS

/// A row from `cas_objects` — bytes plus the declared content type and
/// length. Hash is implicit (you fetched it by hash).
#[derive(Debug, Clone)]
pub struct CasObject {
    pub blake3: String,
    pub byte_len: i64,
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
}

/// One pre-hashed entry to bulk-insert via [`BlobCas::put_many`]. The
/// caller is responsible for computing `blake3` (use [`blake3_hex`])
/// before calling; this struct exists so put_many doesn't have to
/// re-hash the same bytes the caller has already hashed for its own
/// `blob_refs` row.
#[derive(Debug, Clone, Copy)]
pub struct CasInsert<'a> {
    pub blake3: &'a str,
    pub bytes: &'a [u8],
    pub content_type: Option<&'a str>,
}

/// How long a reader waits out a writer's commit. A commit writes bytes
/// already in memory, so it lasts as long as the disk write.
const BUSY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

/// The CAS read-only, as a bare pool, for a render that reads blobs
/// beside its pinned entity store. There is nothing to pin: a reader sees
/// committed transactions only, and every blob commits before the edge
/// row naming it is written (`flush_cas_edges`).
pub async fn open_cas_reader(cas_path: &Path) -> Result<SqlitePool> {
    connect(cas_path, true).await
}

/// The CAS is plain SQLite rather than a doltlite store: it is its own
/// history, since a hash is present or not and a row never changes, so a
/// doltlite commit log would only keep every page it ever rewrote.
async fn connect(cas_path: &Path, read_only: bool) -> Result<SqlitePool> {
    let opts = SqliteConnectOptions::new()
        .filename(datalib_runtime::plain_sqlite::uri(cas_path))
        .read_only(read_only)
        .create_if_missing(!read_only)
        // The blobs-before-edges order survives a power cut only if the
        // blob's commit reached the disk.
        .synchronous(SqliteSynchronous::Full)
        .busy_timeout(BUSY_TIMEOUT);
    SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        .connect_with(opts)
        .await
        .with_context(|| format!("open blob cas {}", cas_path.display()))
}

/// Where the CAS lived while it was a doltlite store. Temporary: remove
/// it, [`convert_a_doltlite_cas`] and their tests in 0.41.
const DOLTLITE_CAS: &str = "blobs.doltlite_db";

/// Moves a CAS an older build left in doltlite's format into the plain
/// file at `cas_path`, then deletes the old one. Left alone, a new empty
/// CAS beside edge rows that name every hash would read as "all fetched"
/// to the download's skip check, and every attachment would render as
/// missing for good.
///
/// The copy goes to a temporary file and is renamed into place only once
/// its row count and byte total match, so a crash leaves the old store
/// whole and the next open starts over.
async fn convert_a_doltlite_cas(cas_path: &Path) -> Result<()> {
    let dir = cas_path.parent().unwrap_or_else(|| Path::new("."));
    let old = dir.join(DOLTLITE_CAS);
    if !old.exists() {
        return Ok(());
    }
    if !cas_path.exists() {
        copy_doltlite_cas(&old, cas_path).await?;
    }
    for leftover in [
        old.clone(),
        dir.join(format!("{DOLTLITE_CAS}.lock")),
        dir.join(format!(".{DOLTLITE_CAS}-lock")),
    ] {
        match std::fs::remove_file(&leftover) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => {
                return Err(e).with_context(|| format!("remove {}", leftover.display()));
            }
            _ => {}
        }
    }
    Ok(())
}

async fn copy_doltlite_cas(old: &Path, cas_path: &Path) -> Result<()> {
    let tmp = cas_path.with_extension("sqlite.tmp");
    for stale in [tmp.clone(), tmp.with_extension("tmp-journal")] {
        if stale.exists() {
            std::fs::remove_file(&stale).with_context(|| format!("remove {}", stale.display()))?;
        }
    }
    // A connection of its own on the old file, not `doltlite_raw::open`:
    // this reads `main`, where every seal published its blobs, and writes
    // nothing to the file it reads.
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .idle_timeout(None)
        .max_lifetime(None)
        // `ATTACH` opens with this connection's flags, and the copy it
        // attaches does not exist yet. `old` does, so nothing is created here.
        .connect_with(
            SqliteConnectOptions::new()
                .filename(old)
                .create_if_missing(true),
        )
        .await
        .with_context(|| format!("open {}", old.display()))?;
    let copied = async {
        sqlx::raw_sql(sqlx::AssertSqlSafe(conversion_sql(&tmp)))
            .execute(&pool)
            .await
            .context("copy the blobs")?;
        // Totals of `length(bytes)` on the new side, not the copied
        // `byte_len`, so a truncated value cannot pass.
        let (rows_old, rows_new, bytes_old, bytes_new): (i64, i64, i64, i64) = sqlx::query_as(
            "SELECT (SELECT count(*) FROM main.cas_objects), \
                    (SELECT count(*) FROM out.cas_objects), \
                    (SELECT coalesce(sum(byte_len), 0) FROM main.cas_objects), \
                    (SELECT coalesce(sum(length(bytes)), 0) FROM out.cas_objects)",
        )
        .fetch_one(&pool)
        .await
        .context("count the copy")?;
        if (rows_old, bytes_old) != (rows_new, bytes_new) {
            anyhow::bail!(
                "the copy holds {rows_new} blobs of {bytes_new} bytes, \
                 the old store {rows_old} of {bytes_old}"
            );
        }
        tracing::info!(
            blobs = rows_new,
            bytes = bytes_new,
            from = %old.display(),
            "converted a doltlite blob store to plain SQLite"
        );
        Ok(())
    }
    .await;
    pool.close().await;
    copied.with_context(|| format!("convert {} to plain SQLite", old.display()))?;
    std::fs::rename(&tmp, cas_path).with_context(|| format!("move {} into place", tmp.display()))
}

/// Copies every blob from the doltlite CAS the connection is on into a
/// new plain CAS at `target`, attached as `out`.
fn conversion_sql(target: &Path) -> String {
    let ddl = CAS_OBJECTS_DDL.replace("IF NOT EXISTS cas_objects", "out.cas_objects");
    format!(
        "ATTACH '{uri}' AS out; PRAGMA out.synchronous = FULL; {ddl}; \
         INSERT INTO out.cas_objects SELECT blake3, byte_len, content_type, bytes FROM main.cas_objects;",
        uri = datalib_runtime::plain_sqlite::uri(target).replace('\'', "''"),
    )
}

/// Per-source CAS handle. Single sqlx pool of size 1, same as every
/// other store in this codebase.
#[derive(Clone, Debug)]
pub struct BlobCas {
    pool: SqlitePool,
}

impl BlobCas {
    /// The download step's handle on its CAS.
    pub async fn open(cas_path: &Path) -> Result<Self> {
        convert_a_doltlite_cas(cas_path).await?;
        let pool = connect(cas_path, false).await?;
        if let Err(e) = sqlx::query(CAS_OBJECTS_DDL).execute(&pool).await {
            pool.close().await;
            return Err(e).context("create cas_objects");
        }
        Ok(Self { pool })
    }

    /// Open the CAS to *read* it, for a render pass.
    pub async fn open_reader(cas_path: &Path) -> Result<Self> {
        Ok(Self {
            pool: open_cas_reader(cas_path).await?,
        })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Wait for the connection to actually go away. Dropping the handle
    /// only schedules that, and a store reopened in the meantime is a
    /// second connection — see the crate README.
    pub async fn close(self) {
        self.pool.close().await;
    }

    pub async fn put(&self, bytes: &[u8], content_type: Option<&str>) -> Result<String> {
        let hash = blake3_hex(bytes);
        sqlx::query(
            "INSERT OR IGNORE INTO cas_objects (blake3, byte_len, content_type, bytes) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(&hash)
        .bind(bytes.len() as i64)
        .bind(content_type)
        .bind(bytes)
        .execute(&self.pool)
        .await
        .context("cas put")?;
        crate::download_metrics::record_upserts("cas_objects", 1);
        Ok(hash)
    }

    /// Bulk-insert pre-hashed bytes in a single transaction, using
    /// chunked multi-row `INSERT OR IGNORE`. The transaction's `COMMIT` is
    /// the blobs' commit: a caller writing edge rows after this returns
    /// names only bytes already on disk.
    pub async fn put_many(&self, items: &[CasInsert<'_>]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.context("begin cas put_many tx")?;
        for chunk in items.chunks(crate::bulk::SQL_CHUNK) {
            let mut sql = String::from(
                "INSERT OR IGNORE INTO cas_objects (blake3, byte_len, content_type, bytes) VALUES ",
            );
            crate::bulk::push_placeholders(&mut sql, chunk.len(), 4);
            // Audited for injection per sqlx 0.9's `SqlSafeStr` bound: `sql` is a
            // `&'static str` prefix plus a `(?,?,?),...` run that `push_placeholders`
            // builds from `chunk.len()`. Every value is bound.
            let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
            for it in chunk {
                q = q
                    .bind(it.blake3)
                    .bind(it.bytes.len() as i64)
                    .bind(it.content_type)
                    .bind(it.bytes);
            }
            q.execute(&mut *tx)
                .await
                .context("bulk insert cas_objects")?;
        }
        tx.commit().await.context("commit cas put_many tx")?;
        // Tally CAS writes against the current source's download metrics
        // (no-op outside an download scope). Counts attempts; some are
        // INSERT-OR-IGNORE dupes, so rows_after - rows_before is the
        // real net-new figure.
        crate::download_metrics::record_upserts("cas_objects", items.len());
        Ok(())
    }

    pub async fn get(&self, blake3_hash: &str) -> Result<Option<CasObject>> {
        let row = sqlx::query(
            "SELECT blake3, byte_len, content_type, bytes FROM cas_objects WHERE blake3 = ?",
        )
        .bind(blake3_hash)
        .fetch_optional(&self.pool)
        .await
        .with_context(|| format!("cas get {blake3_hash}"))?;
        Ok(row.map(row_to_cas_object))
    }
}

fn row_to_cas_object(r: SqliteRow) -> CasObject {
    CasObject {
        blake3: r.try_get("blake3").unwrap_or_default(),
        byte_len: r.try_get("byte_len").unwrap_or_default(),
        content_type: r.try_get("content_type").ok(),
        bytes: r.try_get("bytes").unwrap_or_default(),
    }
}

pub fn blake3_hex(bytes: &[u8]) -> String {
    blake3::hash(bytes).to_hex().to_string()
}

// Read side — every provider now uses [`BlobBundle`] (below). The
// retired `BlobView` / `BlobReader` / `SqliteBlobReader` /
// `InMemoryBlobReader` / `materialize_to_disk` / `materialize_refs` /
// `attachment_md` surface was deleted with the notion port (the last
// consumer); see git history if you need its old shape.

// content-type → extension

/// Pick a file extension from a `content_type` like `image/png` or
/// `application/pdf`. Returns `None` for types we don't have a stable
/// extension for; the caller can fall back to the upstream filename's
/// extension or to no extension at all.
pub fn extension_for_content_type(ct: Option<&str>) -> Option<String> {
    let ct = ct?.split(';').next()?.trim().to_ascii_lowercase();
    let ext = match ct.as_str() {
        "image/png" => "png",
        "image/jpeg" | "image/jpg" => "jpg",
        "image/gif" => "gif",
        "image/webp" => "webp",
        "image/svg+xml" => "svg",
        "image/heic" => "heic",
        "image/heif" => "heif",
        "image/avif" => "avif",
        "image/bmp" => "bmp",
        "image/tiff" => "tiff",
        "application/pdf" => "pdf",
        "application/zip" => "zip",
        "application/json" => "json",
        "application/octet-stream" => return None,
        "text/plain" => "txt",
        "text/markdown" => "md",
        "text/csv" => "csv",
        "text/html" => "html",
        // Calendar invites arrive both as an inline `text/calendar;
        // method=REQUEST` body part (no filename) and as an `invite.ics`
        // attachment part. Without this arm the inline copy derives no
        // extension at all and lands as a bare hex stem.
        "text/calendar" | "text/x-vcalendar" | "application/ics" => "ics",
        "video/mp4" => "mp4",
        "video/quicktime" => "mov",
        "video/webm" => "webm",
        "audio/mpeg" => "mp3",
        "audio/mp4" => "m4a",
        "audio/wav" | "audio/x-wav" => "wav",
        _ => return None,
    };
    Some(ext.to_string())
}

pub fn extension_from_upstream_name(name: Option<&str>) -> Option<String> {
    let name = name?;
    let (_, ext) = name.rsplit_once('.')?;
    if ext.is_empty() || ext.len() > 8 || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

/// Total order over the candidate names refs sharing one content hash
/// derive, used by [`BlobBundle::names_by_blake3`] to pick a winner.
/// Higher wins: a name carrying an extension beats a bare stem, and
/// among equals the lexicographically smallest name wins (`Reverse`).
/// Deliberately total — ties broken by `HashMap` order would make the
/// rendered tree differ run to run.
fn name_rank(name: &str) -> (bool, std::cmp::Reverse<&str>) {
    (name.contains('.'), std::cmp::Reverse(name))
}

// BlobBundle — per-doc unit of attachment data, read + write

/// One attachment's worth of data inside a [`BlobBundle`] — blake3 +
/// bytes + the metadata `rendered_filename` needs.
#[derive(Debug, Clone)]
pub struct Blob {
    pub blake3: String,
    pub bytes: Vec<u8>,
    pub content_type: Option<String>,
    pub upstream_name: Option<String>,
}

impl Blob {
    pub fn rendered_filename(&self) -> String {
        let ext = extension_for_content_type(self.content_type.as_deref())
            .or_else(|| extension_from_upstream_name(self.upstream_name.as_deref()));
        let short = &self.blake3[..16.min(self.blake3.len())];
        match ext {
            Some(e) => format!("{short}.{e}"),
            None => short.to_string(),
        }
    }
}

/// One fetched-but-not-yet-flushed entry on the download side, exposed
/// through [`BlobBundle::fetched_refs`] so the per-provider flush code
/// can build edge-table rows from it.
#[derive(Debug, Clone, Copy)]
pub struct FetchedRef<'a> {
    pub ref_id: &'a str,
    pub blake3: &'a str,
    pub content_type: Option<&'a str>,
    pub upstream_name: Option<&'a str>,
}

/// Per-doc bundle of attachment data. Travels through the whole
/// pipeline:
#[derive(Debug, Clone, Default)]
pub struct BlobBundle {
    by_ref: HashMap<String, Blob>,
    errors: Vec<(String, String)>,
}

impl BlobBundle {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.by_ref.is_empty()
    }

    pub fn len(&self) -> usize {
        self.by_ref.len()
    }

    pub fn get(&self, ref_id: &str) -> Option<&Blob> {
        self.by_ref.get(ref_id)
    }

    pub fn iter(&self) -> impl Iterator<Item = (&str, &Blob)> {
        self.by_ref.iter().map(|(k, v)| (k.as_str(), v))
    }

    // ── download side ─────────────────────────────────────────────────

    /// Record one fetched attachment. `bytes` is hashed lazily —
    /// caller does NOT need to pre-compute blake3.
    pub fn add(
        &mut self,
        ref_id: impl Into<String>,
        bytes: Vec<u8>,
        content_type: Option<String>,
        upstream_name: Option<String>,
    ) {
        let blake3 = blake3_hex(&bytes);
        self.by_ref.insert(
            ref_id.into(),
            Blob {
                blake3,
                bytes,
                content_type,
                upstream_name,
            },
        );
    }

    pub fn add_error(&mut self, ref_id: impl Into<String>, error: impl Into<String>) {
        self.errors.push((ref_id.into(), error.into()));
    }

    pub fn cas_inserts(&self) -> Vec<CasInsert<'_>> {
        self.by_ref
            .values()
            .map(|b| CasInsert {
                blake3: b.blake3.as_str(),
                bytes: b.bytes.as_slice(),
                content_type: b.content_type.as_deref(),
            })
            .collect()
    }

    /// Iterator over fetched refs in arbitrary order — caller maps
    /// these into per-provider edge-table row structs.
    pub fn fetched_refs(&self) -> impl Iterator<Item = FetchedRef<'_>> {
        self.by_ref.iter().map(|(ref_id, b)| FetchedRef {
            ref_id: ref_id.as_str(),
            blake3: b.blake3.as_str(),
            content_type: b.content_type.as_deref(),
            upstream_name: b.upstream_name.as_deref(),
        })
    }

    pub fn errors(&self) -> &[(String, String)] {
        &self.errors
    }

    // ── parse side ───────────────────────────────────────────────────

    /// A bundle for each key that names at least one ref, from one query
    /// over every key's refs rather than one per key.
    pub async fn load_many<K, R>(
        refs_pool: &SqlitePool,
        cas_pool: &SqlitePool,
        projection_sql_template: &str,
        refs_by_key: impl IntoIterator<Item = (K, R)>,
    ) -> Result<HashMap<K, Self>>
    where
        K: Eq + Hash + Clone,
        R: IntoIterator,
        R::Item: Into<String>,
    {
        let mut out: HashMap<K, Self> = HashMap::new();
        let mut keys_by_ref: HashMap<String, Vec<K>> = HashMap::new();
        for (key, refs) in refs_by_key {
            for r in refs {
                let keys = keys_by_ref.entry(r.into()).or_default();
                if keys.last() != Some(&key) {
                    keys.push(key.clone());
                }
                out.entry(key.clone()).or_default();
            }
        }
        if keys_by_ref.is_empty() {
            return Ok(out);
        }

        // Stage 1: ref_id → (blake3, content_type, upstream_name). The
        // template may name `{placeholders}` more than once (email's UNION
        // ALL does); each gets the whole list.
        let ref_ids = serde_json::to_string(&keys_by_ref.keys().collect::<Vec<_>>())?;
        let occurrences = projection_sql_template.matches("{placeholders}").count();
        let sql =
            projection_sql_template.replace("{placeholders}", "SELECT value FROM json_each(?)");
        // Audited for injection per sqlx 0.9's `SqlSafeStr` bound: the template is
        // caller-supplied, but every caller passes a module-level `const &str`
        // literal (`*_PROJECTION*` in the providers); the only substitution is
        // `{placeholders}` -> a fixed `json_each` over one bound parameter.
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for _ in 0..occurrences {
            q = q.bind(&ref_ids);
        }
        let rows = q
            .fetch_all(refs_pool)
            .await
            .context("BlobBundle::load_many projection")?;
        struct PendingEntry {
            ref_id: String,
            content_type: Option<String>,
            upstream_name: Option<String>,
        }
        let mut pending_by_blake3: HashMap<String, Vec<PendingEntry>> = HashMap::new();
        for r in &rows {
            let Ok(ref_id) = r.try_get::<String, _>("ref_id") else {
                continue;
            };
            let Ok(blake3) = r.try_get::<String, _>("blake3") else {
                continue;
            };
            pending_by_blake3
                .entry(blake3)
                .or_default()
                .push(PendingEntry {
                    ref_id,
                    content_type: r.try_get("content_type").ok().flatten(),
                    upstream_name: r.try_get("upstream_name").ok().flatten(),
                });
        }
        drop(rows);

        // Stage 2: the bytes, a chunk at a time, each handed out before the
        // next is read. `blake3` is `cas_objects`' key, so a chunk is an
        // index lookup, not a scan.
        let hashes: Vec<String> = pending_by_blake3.keys().cloned().collect();
        for chunk in hashes.chunks(crate::bulk::SQL_CHUNK) {
            let cas_rows = sqlx::query(
                "SELECT blake3, bytes, content_type FROM cas_objects \
                  WHERE blake3 IN (SELECT value FROM json_each(?))",
            )
            .bind(serde_json::to_string(chunk)?)
            .fetch_all(cas_pool)
            .await
            .context("BlobBundle::load_many cas_objects")?;
            for cr in &cas_rows {
                let Ok(blake3) = cr.try_get::<String, _>("blake3") else {
                    continue;
                };
                let Some(entries) = pending_by_blake3.remove(&blake3) else {
                    continue;
                };
                let bytes: Vec<u8> = cr.try_get("bytes").unwrap_or_default();
                let cas_ct: Option<String> = cr.try_get("content_type").ok().flatten();
                for entry in entries {
                    for key in &keys_by_ref[&entry.ref_id] {
                        out.entry(key.clone()).or_default().by_ref.insert(
                            entry.ref_id.clone(),
                            Blob {
                                blake3: blake3.clone(),
                                bytes: bytes.clone(),
                                content_type: entry.content_type.clone().or_else(|| cas_ct.clone()),
                                upstream_name: entry.upstream_name.clone(),
                            },
                        );
                    }
                }
            }
        }
        Ok(out)
    }

    // ── render side (sync) ───────────────────────────────────────────

    fn names_by_blake3(&self) -> BTreeMap<&str, String> {
        let mut out: BTreeMap<&str, String> = BTreeMap::new();
        for blob in self.by_ref.values() {
            let cand = blob.rendered_filename();
            match out.entry(blob.blake3.as_str()) {
                Entry::Vacant(v) => {
                    v.insert(cand);
                }
                Entry::Occupied(mut o) => {
                    if name_rank(&cand) > name_rank(o.get()) {
                        o.insert(cand);
                    }
                }
            }
        }
        out
    }

    /// The name `ref_id`'s bytes are materialized under, deduped across
    /// every ref in the bundle sharing those bytes. Callers building a
    /// `blobs/<file>` link MUST use this rather than
    /// [`Blob::rendered_filename`], or the link can name a file
    /// [`Self::materialize_to_dir`] chose not to write.
    pub fn filename_for(&self, ref_id: &str) -> Option<String> {
        let blob = self.by_ref.get(ref_id)?;
        // Only this ref's hash-mates matter, so scan for the winner
        // directly instead of building the whole map for one lookup.
        let mut best = blob.rendered_filename();
        for other in self.by_ref.values() {
            if other.blake3 != blob.blake3 {
                continue;
            }
            let cand = other.rendered_filename();
            if name_rank(&cand) > name_rank(&best) {
                best = cand;
            }
        }
        Some(best)
    }

    /// Write each distinct payload's bytes into
    /// `blobs_dir/<filename_for(ref)>`, once — refs sharing a content
    /// hash share the one file. Skips a write when the target already
    /// exists with the expected size, so re-running render against the
    /// same bundle is idempotent.
    pub fn materialize_to_dir(&self, blobs_dir: &Path) -> std::io::Result<()> {
        if self.by_ref.is_empty() {
            return Ok(());
        }
        std::fs::create_dir_all(blobs_dir)?;
        let names = self.names_by_blake3();
        let mut by_hash: HashMap<&str, &Blob> = HashMap::new();
        for blob in self.by_ref.values() {
            by_hash.entry(blob.blake3.as_str()).or_insert(blob);
        }
        for (hash, fname) in &names {
            let Some(blob) = by_hash.get(hash) else {
                continue;
            };
            let abs = blobs_dir.join(fname);
            if let Ok(meta) = std::fs::metadata(&abs) {
                if meta.len() == blob.bytes.len() as u64 {
                    continue;
                }
            }
            std::fs::write(&abs, &blob.bytes)?;
        }
        Ok(())
    }

    /// Emit `![alt](blobs/<file>)` for images, `[\[file\] alt](…)`
    /// otherwise. Returns the "attachment not yet fetched" placeholder
    /// when the bundle has no entry for this `ref_id`.
    pub fn markdown_link(&self, ref_id: &str, display: Option<&str>, is_image: bool) -> String {
        let Some(blob) = self.by_ref.get(ref_id) else {
            let label = display.unwrap_or(ref_id);
            return format!("*[attachment not yet fetched: {label}]*");
        };
        let fname = self
            .filename_for(ref_id)
            .unwrap_or_else(|| blob.rendered_filename());
        let display_clean = display.unwrap_or("").replace(']', "");
        let alt = if display_clean.is_empty() {
            fname.clone()
        } else {
            display_clean
        };
        let link = format!("blobs/{fname}");
        if is_image {
            format!("![{alt}]({link})")
        } else {
            format!("[\\[file\\] {alt}]({link})")
        }
    }
}

// Per-provider CAS-edge tables — shared shape

/// The shape every per-provider CAS edge table follows. One row per
/// `(owning_id, ref_id)` pair, recording the CAS `blake3` for the
/// bytes that the upstream's `ref_id` resolved to.
pub trait CasEdgeRow: crate::bulk::BulkUpsertable {
    /// SQL column name carrying the owning-entity FK
    /// (e.g. `conversation_id`, `message_uuid`, `chat_item_id`).
    const OWNING_COLUMN: &'static str;
    /// SQL column name carrying the upstream ref id
    /// (e.g. `file_id`, `file_uuid`, `ref_id`).
    const REF_COLUMN: &'static str;

    /// `CREATE TABLE IF NOT EXISTS …` for this edge table. Same shape
    /// for every provider — `id` PK, owning FK NOT NULL, ref NOT
    /// NULL, blake3 nullable hex.
    fn ddl() -> String {
        format!(
            "CREATE TABLE IF NOT EXISTS {table} (
    id      TEXT PRIMARY KEY,
    {owning} TEXT NOT NULL,
    {ref_c}  TEXT NOT NULL,
    blake3  TEXT NULL,
    CHECK (blake3 IS NULL OR length(blake3) = 64)
)",
            table = Self::TABLE,
            owning = Self::OWNING_COLUMN,
            ref_c = Self::REF_COLUMN,
        )
    }

    fn by_owning_index_ddl() -> String {
        format!(
            "CREATE INDEX IF NOT EXISTS {table}_by_{owning} ON {table}({owning})",
            table = Self::TABLE,
            owning = Self::OWNING_COLUMN,
        )
    }

    /// Index on `(ref_column, blake3)` — supports the download's skip-check
    /// "have we ever stored this ref's bytes" without a full scan.
    fn by_ref_index_ddl() -> String {
        format!(
            "CREATE INDEX IF NOT EXISTS {table}_by_{ref_c} ON {table}({ref_c}, blake3)",
            table = Self::TABLE,
            ref_c = Self::REF_COLUMN,
        )
    }

    fn all_ddl() -> Vec<String> {
        vec![
            Self::ddl(),
            Self::by_owning_index_ddl(),
            Self::by_ref_index_ddl(),
        ]
    }

    /// Synthesized primary key recipe: `"{owning_id}#{ref_id}"`.
    /// Universal across all four providers, so it lives here once.
    fn pk_recipe(owning_id: &str, ref_id: &str) -> String {
        format!("{owning_id}#{ref_id}")
    }

    /// The owning id back out of a [`pk_recipe`](Self::pk_recipe) key.
    /// An owning id may hold `#` (Slack's does); a ref id must not.
    fn owning_id_of(pk: &str) -> Option<&str> {
        pk.rsplit_once('#').map(|(owning, _)| owning)
    }
}

// Per-provider CAS-edge index loader

/// Snapshot a per-provider CAS edge table as a `(ref_id → blake3)`
/// in-memory map. Loaded once at the start of `fetch()` so the
/// per-file "have we got these bytes yet?" check is a HashMap hit
/// instead of a SQLite round trip per file.
pub async fn load_blake3_index(
    pool: &SqlitePool,
    table: &str,
    ref_id_column: &str,
) -> Result<HashMap<String, String>> {
    let sql = format!(
        "SELECT {ref_id_column} AS ref_id, blake3 FROM {table} \
          WHERE blake3 IS NOT NULL"
    );
    // Audited for injection per sqlx 0.9's `SqlSafeStr` bound: `table` and
    // `ref_id_column` are interpolated as bare identifiers, so they must stay
    // literals. All three callers pass `&'static str` (claude/slack/chatgpt
    // attachment tables); do not pass user input here.
    let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
        .fetch_all(pool)
        .await
        .with_context(|| format!("load_blake3_index {table}.{ref_id_column}"))?;
    let mut out: HashMap<String, String> = HashMap::with_capacity(rows.len());
    for r in &rows {
        let Ok(ref_id) = r.try_get::<String, _>("ref_id") else {
            continue;
        };
        let Ok(blake3) = r.try_get::<String, _>("blake3") else {
            continue;
        };
        if !ref_id.is_empty() && !blake3.is_empty() {
            out.entry(ref_id).or_insert(blake3);
        }
    }
    Ok(out)
}

// CAS-edge accumulator

/// Per-bucket attachment-fetch accumulator. Every per-provider
/// download walks an upstream bucket (a conversation, a thread, a
/// channel of messages) and decides per file: did we just fetch
/// bytes, did we discover bytes were already in the CAS, or did
/// the fetch fail? This struct collects those outcomes, then
/// [`Self::flush`] hands them to [`flush_cas_edges`] via a row
/// builder the caller supplies.
pub struct CasEdgeAccumulator {
    bundle: BlobBundle,
    edges: Vec<EdgePending>,
    /// Per blob: why it has no bytes. A failure and a deliberate skip
    /// both land here, and they are told apart by the `Reason`.
    errors: Vec<BlobNotFetched>,
    seen: std::collections::HashSet<(String, String)>,
    known_blake3: HashMap<String, String>,
}

struct EdgePending {
    owning_id: String,
    ref_id: String,
}

/// One blob that ended the run without bytes, and why.
#[derive(Debug, Clone)]
pub struct BlobNotFetched {
    pub ref_id: String,
    pub detail: String,
    /// `FetchFailed` for something that went wrong; anything else is a
    /// rule we applied on purpose.
    pub reason: datalib_problems::Reason,
}

impl CasEdgeAccumulator {
    pub fn new() -> Self {
        Self {
            bundle: BlobBundle::new(),
            edges: Vec::new(),
            errors: Vec::new(),
            seen: std::collections::HashSet::new(),
            known_blake3: HashMap::new(),
        }
    }

    /// Direct mutable access to the underlying [`BlobBundle`] —
    /// rarely needed; here for callers that want to set
    /// upstream_name / content_type via the bundle's own helpers
    /// without round-tripping through [`Self::add_fetched`].
    pub fn bundle_mut(&mut self) -> &mut BlobBundle {
        &mut self.bundle
    }

    fn push_edge(&mut self, owning_id: &str, ref_id: &str) -> bool {
        if !self
            .seen
            .insert((owning_id.to_string(), ref_id.to_string()))
        {
            return false;
        }
        self.edges.push(EdgePending {
            owning_id: owning_id.to_string(),
            ref_id: ref_id.to_string(),
        });
        true
    }

    pub fn add_fetched(
        &mut self,
        owning_id: &str,
        ref_id: &str,
        bytes: Vec<u8>,
        content_type: Option<String>,
        upstream_name: Option<String>,
    ) {
        self.push_edge(owning_id, ref_id);
        self.bundle.add(ref_id, bytes, content_type, upstream_name);
    }

    pub fn add_known(&mut self, owning_id: &str, ref_id: &str, blake3: String) {
        self.push_edge(owning_id, ref_id);
        self.known_blake3
            .entry(ref_id.to_string())
            .or_insert(blake3);
    }

    pub fn add_failed(&mut self, owning_id: &str, ref_id: &str, err: impl Into<String>) {
        self.push_edge(owning_id, ref_id);
        self.errors.push(BlobNotFetched {
            ref_id: ref_id.to_string(),
            detail: err.into(),
            reason: datalib_problems::Reason::FetchFailed,
        });
        self.bundle.add_error(ref_id, "fetch failed");
    }

    /// A blob the download declined to fetch, because a rule in the
    /// config said not to. Not a failure, but a warning all the same:
    /// the mirror is missing the file.
    ///
    /// The bookkeeping is a failure's, which is deliberate — it keeps
    /// the blob where a provider's retry pass looks for what did not
    /// land. See `doltlite_raw::record_object_skipped`.
    pub fn add_skipped(
        &mut self,
        owning_id: &str,
        ref_id: &str,
        reason: datalib_problems::Reason,
        detail: impl Into<String>,
    ) {
        self.push_edge(owning_id, ref_id);
        self.errors.push(BlobNotFetched {
            ref_id: ref_id.to_string(),
            detail: detail.into(),
            reason,
        });
        self.bundle.add_error(ref_id, "not fetched");
    }

    pub async fn flush<T, F>(
        &self,
        pool: &sqlx::SqlitePool,
        cas: &BlobCas,
        build_row: F,
    ) -> Result<()>
    where
        T: crate::bulk::BulkUpsertable,
        F: Fn(&str, &str, Option<&str>) -> T,
    {
        let mut blake3_by_ref: HashMap<&str, &str> = HashMap::new();
        for f in self.bundle.fetched_refs() {
            blake3_by_ref.insert(f.ref_id, f.blake3);
        }
        for (ref_id, hash) in &self.known_blake3 {
            blake3_by_ref
                .entry(ref_id.as_str())
                .or_insert(hash.as_str());
        }

        let rows: Vec<T> = self
            .edges
            .iter()
            .map(|e| {
                build_row(
                    &e.owning_id,
                    &e.ref_id,
                    blake3_by_ref.get(e.ref_id.as_str()).copied(),
                )
            })
            .collect();

        // Error stamps: expand (ref_id → err) failures into per-edge
        // (synth_pk, err) bookkeeping stamps.
        let row_id_by_index: Vec<(String, String)> = rows
            .iter()
            .zip(self.edges.iter())
            .map(|(row, edge)| (row.id().to_string(), edge.ref_id.clone()))
            .collect();
        let mut error_stamps: Vec<BlobNotFetched> = Vec::new();
        for problem in &self.errors {
            for (row_id, edge_ref_id) in &row_id_by_index {
                if *edge_ref_id == problem.ref_id {
                    error_stamps.push(BlobNotFetched {
                        ref_id: row_id.clone(),
                        ..problem.clone()
                    });
                }
            }
        }

        flush_cas_edges(pool, cas, &self.bundle.cas_inserts(), rows, &error_stamps).await
    }
}

impl Default for CasEdgeAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

// CAS-edge flush primitive

/// End-of-bucket CAS-edge flush. The shape every per-provider CAS
/// edge table (chatgpt_attachments, claude_attachments,
/// slack_attachments, chat_item_attachments) used to hand-roll
/// individually:
pub async fn flush_cas_edges<T: crate::bulk::BulkUpsertable>(
    pool: &SqlitePool,
    cas: &BlobCas,
    cas_inserts: &[CasInsert<'_>],
    rows: Vec<T>,
    errors: &[BlobNotFetched],
) -> Result<()> {
    if rows.is_empty() && cas_inserts.is_empty() && errors.is_empty() {
        return Ok(());
    }
    if !cas_inserts.is_empty() {
        cas.put_many(cas_inserts)
            .await
            .with_context(|| format!("flush_cas_edges put_many {}", T::TABLE))?;
    }
    let now = datalib_time::IsoOffsetTimestamp::now_local();
    let mut tx = pool
        .begin()
        .await
        .with_context(|| format!("begin flush_cas_edges {} tx", T::TABLE))?;
    // Every edge gets its row, but only one that landed is stamped
    // fetched: the stamp is how a later failure tells a stale copy from a
    // record that never arrived. An edge that failed this time but already
    // points at bytes keeps them: a failed read is not news that the file
    // changed, and writing it again would leave the bytes unreachable.
    let not_fetched: HashSet<&str> = errors.iter().map(|e| e.ref_id.as_str()).collect();
    let holding = edges_holding_bytes::<T>(&mut tx, &not_fetched).await?;
    let rows: Vec<T> = rows
        .into_iter()
        .filter(|r| !holding.contains(r.id()))
        .collect();
    crate::bulk::bulk_upsert_entity_in_tx(&mut tx, &rows).await?;
    crate::bulk::bulk_upsert_bookkeeping(
        &mut tx,
        T::TABLE,
        rows.iter()
            .map(|r| r.id())
            .filter(|id| !not_fetched.contains(id)),
        &now,
    )
    .await?;
    for problem in errors {
        match problem.reason {
            datalib_problems::Reason::FetchFailed => {
                crate::doltlite_raw::record_object_attempt(
                    &mut tx,
                    T::TABLE,
                    &problem.ref_id,
                    Some(&problem.detail),
                )
                .await?
            }
            reason => {
                crate::doltlite_raw::record_object_skipped(
                    &mut tx,
                    T::TABLE,
                    &problem.ref_id,
                    reason,
                    &problem.detail,
                )
                .await?
            }
        }
    }
    tx.commit()
        .await
        .with_context(|| format!("commit flush_cas_edges {} tx", T::TABLE))?;
    Ok(())
}

/// Which of `ids` already have a stored edge that points at bytes.
async fn edges_holding_bytes<T: crate::bulk::BulkUpsertable>(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    ids: &HashSet<&str>,
) -> Result<HashSet<String>> {
    let ids: Vec<&str> = ids.iter().copied().collect();
    let mut out = HashSet::new();
    for chunk in ids.chunks(crate::bulk::SQL_CHUNK) {
        let mut placeholders = String::new();
        crate::bulk::push_placeholder_list(&mut placeholders, chunk.len());
        let sql = format!(
            "SELECT id FROM {} WHERE blake3 IS NOT NULL AND id IN ({placeholders})",
            T::TABLE
        );
        // Audited: the table is the row type's `&'static str`; the IN-list
        // is a `?,?,?` run sized from the chunk and every id is bound.
        let mut q = sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(sql));
        for id in chunk {
            q = q.bind(*id);
        }
        out.extend(
            q.fetch_all(&mut **tx)
                .await
                .with_context(|| format!("edges of {} that hold bytes", T::TABLE))?,
        );
    }
    Ok(out)
}

// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    use tempfile::tempdir;

    /// The CAS is a stock SQLite file, not doltlite's format: that is the
    /// whole of why it no longer grows with every checkpoint.
    #[tokio::test]
    async fn a_new_cas_is_plain_sqlite() {
        let d = tempdir().unwrap();
        let path = d.path().join(crate::raw_layout::BLOBS_DB);
        let cas = BlobCas::open(&path).await.unwrap();
        cas.put(b"hello", None).await.unwrap();
        cas.close().await;
        let head = std::fs::read(&path).unwrap();
        assert_eq!(&head[..16], b"SQLite format 3\0");
    }

    /// A CAS as an older build left it: doltlite, with one blob sealed
    /// and published, and its writer lock beside it.
    async fn doltlite_cas_with(dir: &Path, bytes: &[u8]) -> String {
        let pool = crate::doltlite_raw::open(&dir.join(DOLTLITE_CAS), &[CAS_OBJECTS_DDL])
            .await
            .unwrap();
        let hash = blake3_hex(bytes);
        sqlx::query("INSERT INTO cas_objects VALUES (?, ?, 'text/plain', ?)")
            .bind(&hash)
            .bind(bytes.len() as i64)
            .bind(bytes)
            .execute(&pool)
            .await
            .unwrap();
        crate::doltlite_raw::commit_run(&pool, "download: blobs")
            .await
            .unwrap();
        pool.close().await;
        hash
    }

    /// An upgraded root keeps its attachments: the open moves every blob
    /// into the plain file and deletes the doltlite store and its locks,
    /// rather than starting an empty CAS the edge rows would lie about.
    #[tokio::test]
    async fn a_doltlite_cas_is_converted_once_and_the_old_store_goes() {
        let d = tempdir().unwrap();
        let hash = doltlite_cas_with(d.path(), b"kept").await;
        let new_path = d.path().join(crate::raw_layout::BLOBS_DB);

        let cas = BlobCas::open(&new_path).await.unwrap();
        let got = cas.get(&hash).await.unwrap().expect("carried across");
        assert_eq!(got.bytes, b"kept");
        assert_eq!(got.content_type.as_deref(), Some("text/plain"));
        cas.close().await;

        let left: Vec<String> = std::fs::read_dir(d.path())
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, [crate::raw_layout::BLOBS_DB], "nothing else left");
        let head = std::fs::read(&new_path).unwrap();
        assert_eq!(&head[..16], b"SQLite format 3\0");
    }

    /// A conversion killed part-way leaves a temporary copy and maybe its
    /// journal; the next open throws both away and copies again.
    #[tokio::test]
    async fn a_conversion_cut_short_starts_over() {
        let d = tempdir().unwrap();
        let hash = doltlite_cas_with(d.path(), b"kept").await;
        std::fs::write(d.path().join("blobs.sqlite.tmp"), b"half a copy").unwrap();
        std::fs::write(d.path().join("blobs.sqlite.tmp-journal"), b"hot").unwrap();

        let cas = BlobCas::open(&d.path().join(crate::raw_layout::BLOBS_DB))
            .await
            .unwrap();
        assert!(cas.get(&hash).await.unwrap().is_some());
        cas.close().await;
        assert!(!d.path().join("blobs.sqlite.tmp").exists());
        assert!(!d.path().join("blobs.sqlite.tmp-journal").exists());
    }

    /// A conversion that renamed its copy into place and died before
    /// deleting the old store is finished by the next open, which keeps
    /// the new file as it is.
    #[tokio::test]
    async fn an_old_store_beside_a_finished_conversion_is_deleted_not_copied() {
        let d = tempdir().unwrap();
        let new_path = d.path().join(crate::raw_layout::BLOBS_DB);
        let cas = BlobCas::open(&new_path).await.unwrap();
        let kept = cas.put(b"already here", None).await.unwrap();
        cas.close().await;
        let stale = doltlite_cas_with(d.path(), b"only in the old store").await;

        let cas = BlobCas::open(&new_path).await.unwrap();
        assert!(cas.get(&kept).await.unwrap().is_some());
        assert!(cas.get(&stale).await.unwrap().is_none(), "not copied again");
        cas.close().await;
        assert!(!d.path().join(DOLTLITE_CAS).exists());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cas_put_is_idempotent() {
        let d = tempdir().unwrap();
        let cas = BlobCas::open(&d.path().join("blobs.sqlite")).await.unwrap();
        let h1 = cas.put(b"hello", Some("text/plain")).await.unwrap();
        let h2 = cas.put(b"hello", Some("text/plain")).await.unwrap();
        assert_eq!(h1, h2);
        let got = cas.get(&h1).await.unwrap().unwrap();
        assert_eq!(got.bytes, b"hello");
    }

    #[test]
    fn cas_path_for_is_sibling_inside_dir() {
        let p = Path::new("/tmp/raw/slack/entities.doltlite_db");
        assert_eq!(
            cas_path_for(p),
            PathBuf::from("/tmp/raw/slack/blobs.sqlite")
        );
    }

    // ── BlobBundle ──────────────────────────────────────────────────

    #[test]
    fn bundle_add_then_get() {
        let mut b = BlobBundle::new();
        b.add(
            "ref-1",
            b"hello".to_vec(),
            Some("text/plain".into()),
            Some("greeting.txt".into()),
        );
        let got = b.get("ref-1").expect("present");
        assert_eq!(got.blake3.len(), 64);
        assert_eq!(got.bytes, b"hello");
        assert_eq!(got.content_type.as_deref(), Some("text/plain"));
    }

    #[test]
    fn bundle_cas_inserts_round_trip() {
        let mut b = BlobBundle::new();
        b.add("r1", b"aaa".to_vec(), Some("image/png".into()), None);
        b.add("r2", b"bbb".to_vec(), None, Some("x.bin".into()));
        let inserts = b.cas_inserts();
        assert_eq!(inserts.len(), 2);
        // ensure both blake3s are 64-hex
        for i in &inserts {
            assert_eq!(i.blake3.len(), 64);
        }
    }

    #[test]
    fn bundle_markdown_link_placeholder_when_missing() {
        let b = BlobBundle::new();
        let s = b.markdown_link("missing", Some("doc.pdf"), false);
        assert!(s.contains("not yet fetched"));
        assert!(s.contains("doc.pdf"));
    }

    #[test]
    fn bundle_markdown_link_image_when_present() {
        let mut b = BlobBundle::new();
        b.add(
            "img-1",
            b"\x89PNG\r\n\x1a\n".to_vec(),
            Some("image/png".into()),
            Some("kitten.png".into()),
        );
        let s = b.markdown_link("img-1", Some("kitten.png"), true);
        assert!(s.starts_with("![kitten.png](blobs/"));
        assert!(s.ends_with(".png)"));
    }

    /// `text/calendar` was simply missing from the extension table, so
    /// an inline invite part derived no extension at all.
    #[test]
    fn calendar_content_type_yields_ics() {
        assert_eq!(
            extension_for_content_type(Some("text/calendar; method=REQUEST")).as_deref(),
            Some("ics")
        );
    }

    /// The shape that produced byte-identical `blobs/<stem>` +
    /// `blobs/<stem>.ics` pairs: one payload reaching the bundle under
    /// two refs whose metadata derives different extensions.
    #[test]
    fn identical_bytes_under_two_refs_resolve_to_one_name() {
        const BYTES: &[u8] = b"one payload, two refs";
        let mut b = BlobBundle::new();
        // An inline MIME part whose type maps to no extension, with no
        // upstream filename to fall back to: a bare hex stem.
        b.add(
            "inline:x",
            BYTES.to_vec(),
            Some("application/x-thing".into()),
            None,
        );
        // The same bytes again as a named attachment part.
        b.add("att:x", BYTES.to_vec(), None, Some("report.pdf".into()));
        let inline = b.filename_for("inline:x").unwrap();
        let att = b.filename_for("att:x").unwrap();
        assert_eq!(inline, att, "one payload, one name");
        assert!(att.ends_with(".pdf"), "keeps the usable extension: {att}");
        assert_eq!(b.names_by_blake3().len(), 1, "one file per content hash");
    }

    /// The winner must not come from `HashMap` iteration order, or the
    /// rendered tree differs run to run.
    #[test]
    fn resolved_name_is_deterministic_across_insertion_orders() {
        const BYTES: &[u8] = b"same-payload-either-way";
        let build = |flip: bool| {
            let mut b = BlobBundle::new();
            let refs: [(&str, Option<String>, Option<String>); 3] = [
                ("a", Some("application/octet-stream".into()), None),
                ("b", None, Some("report.pdf".into())),
                ("c", None, Some("report.zip".into())),
            ];
            let order: Vec<usize> = if flip { vec![2, 1, 0] } else { vec![0, 1, 2] };
            for i in order {
                let (r, ct, name) = &refs[i];
                b.add(*r, BYTES.to_vec(), ct.clone(), name.clone());
            }
            b
        };
        let fwd = build(false);
        let rev = build(true);
        for r in ["a", "b", "c"] {
            assert_eq!(fwd.filename_for(r), rev.filename_for(r), "ref {r}");
            assert_eq!(fwd.filename_for("a"), fwd.filename_for(r), "ref {r}");
        }
        // `.pdf` < `.zip` lexicographically, and both beat the bare stem
        // `application/octet-stream` alone would give.
        assert!(fwd.filename_for("a").unwrap().ends_with(".pdf"));
    }

    /// Every ref sharing a payload writes one file, and `markdown_link`
    /// points at it from both sides.
    #[test]
    fn materialize_writes_shared_payload_once() {
        let d = tempdir().unwrap();
        const BYTES: &[u8] = b"one payload, two refs";
        let mut b = BlobBundle::new();
        b.add(
            "inline:x",
            BYTES.to_vec(),
            Some("application/x-thing".into()),
            None,
        );
        b.add("att:x", BYTES.to_vec(), None, Some("report.pdf".into()));
        let blobs_dir = d.path().join("blobs");
        b.materialize_to_dir(&blobs_dir).unwrap();
        let mut names: Vec<String> = std::fs::read_dir(&blobs_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        assert_eq!(names.len(), 1, "one file, not two: {names:?}");
        let written = format!("blobs/{}", names[0]);
        for r in ["inline:x", "att:x"] {
            let link = b.markdown_link(r, Some("report.pdf"), false);
            assert!(
                link.contains(&written),
                "link for {r} must name the written file: {link}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bundle_materialize_writes_files() {
        let d = tempdir().unwrap();
        let mut b = BlobBundle::new();
        b.add("r1", b"alpha".to_vec(), Some("image/png".into()), None);
        b.add("r2", b"beta".to_vec(), Some("text/plain".into()), None);
        let blobs_dir = d.path().join("blobs");
        b.materialize_to_dir(&blobs_dir).unwrap();
        let entries: Vec<_> = std::fs::read_dir(&blobs_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .collect();
        assert_eq!(entries.len(), 2);
        // names are <short blake3>.<ext>
        for p in &entries {
            let name = p.file_name().unwrap().to_string_lossy();
            assert!(name.contains('.'), "expected ext in {name}");
        }
    }

    /// One read serves every key: each gets the refs it named and nothing
    /// else, a ref two keys share reaches both, and a template naming
    /// `{placeholders}` twice gets the list in both places.
    #[tokio::test(flavor = "multi_thread")]
    async fn bundle_load_many_round_trips_through_cas() {
        let d = tempdir().unwrap();
        let cas_path = d.path().join("blobs.sqlite");
        let cas = BlobCas::open(&cas_path).await.unwrap();
        // CAS side: stash two blobs.
        let h1 = cas.put(b"alpha", Some("image/png")).await.unwrap();
        let h2 = cas.put(b"beta", Some("application/pdf")).await.unwrap();
        // Refs side: an inline mini edge table mimicking a per-provider
        // attachments table, with (ref_id, blake3, content_type,
        // upstream_name) columns.
        let refs_path = d.path().join("refs.sqlite");
        let opts = sqlx::sqlite::SqliteConnectOptions::from_str(&format!(
            "sqlite://{}",
            refs_path.display()
        ))
        .unwrap()
        .create_if_missing(true);
        let refs_pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .idle_timeout(None)
            .max_lifetime(None)
            .connect_with(opts)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE attachments (
                file_id TEXT PRIMARY KEY,
                blake3 TEXT NOT NULL,
                upstream_name TEXT
            )",
        )
        .execute(&refs_pool)
        .await
        .unwrap();
        for (ref_id, blake3, name) in [
            ("a", h1.as_str(), Some("alpha.png")),
            ("b", h2.as_str(), Some("beta.pdf")),
        ] {
            sqlx::query(
                "INSERT INTO attachments (file_id, blake3, upstream_name) VALUES (?, ?, ?)",
            )
            .bind(ref_id)
            .bind(blake3)
            .bind(name)
            .execute(&refs_pool)
            .await
            .unwrap();
        }

        // `{placeholders}` twice, as email's UNION ALL has it: each gets
        // the whole list.
        let bundles = BlobBundle::load_many(
            &refs_pool,
            cas.pool(),
            "SELECT file_id AS ref_id, blake3, NULL AS content_type, upstream_name \
               FROM attachments WHERE file_id IN ({placeholders}) AND file_id = 'a' \
             UNION ALL \
             SELECT file_id AS ref_id, blake3, NULL AS content_type, upstream_name \
               FROM attachments WHERE file_id IN ({placeholders}) AND file_id <> 'a'",
            [
                ("one", vec!["a", "b", "missing"]),
                ("two", vec!["b"]),
                ("absent", vec!["missing"]),
                ("none", vec![]),
            ],
        )
        .await
        .unwrap();
        let one = &bundles["one"];
        assert_eq!(one.len(), 2);
        let a = one.get("a").expect("a present");
        assert_eq!(a.bytes, b"alpha");
        // content_type comes from CAS when projection doesn't supply it
        assert_eq!(a.content_type.as_deref(), Some("image/png"));
        assert_eq!(a.upstream_name.as_deref(), Some("alpha.png"));
        assert!(one.get("missing").is_none());
        assert_eq!(
            bundles["two"].get("b").map(|b| b.bytes.as_slice()),
            Some(&b"beta"[..]),
            "a ref two keys name reaches both"
        );
        assert!(bundles["absent"].is_empty(), "named refs, found none");
        assert!(
            !bundles.contains_key("none"),
            "a key that names no ref gets no bundle"
        );
    }

    struct WidgetBlob {
        id: String,
        owner: String,
        blake3: Option<String>,
    }

    impl crate::bulk::BulkUpsertable for WidgetBlob {
        const TABLE: &'static str = "widget_blobs";
        const TYPED_COLUMNS: &'static [&'static str] = &["owner", "blake3"];
        const PAYLOAD_COLUMN: Option<&'static str> = None;
        fn id(&self) -> &str {
            &self.id
        }
        fn bind_into<'q>(
            &'q self,
            q: sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments>,
        ) -> sqlx::query::Query<'q, sqlx::Sqlite, sqlx::sqlite::SqliteArguments> {
            q.bind(&self.id).bind(&self.owner).bind(&self.blake3)
        }
    }

    async fn flush_widget_blobs(pool: &SqlitePool, cas: &BlobCas, acc: &CasEdgeAccumulator) {
        acc.flush(pool, cas, |owner, ref_id, blake3| WidgetBlob {
            id: format!("{owner}#{ref_id}"),
            owner: owner.to_string(),
            blake3: blake3.map(String::from),
        })
        .await
        .unwrap();
    }

    async fn fetch_problem(pool: &SqlitePool, id: &str) -> (String, String) {
        sqlx::query_as("SELECT severity, outcome FROM problems WHERE scope_key = ?")
            .bind(format!("widget_blobs:{id}"))
            .fetch_one(pool)
            .await
            .unwrap()
    }

    /// A blob that never landed is dropped, an error; one that landed on
    /// an earlier flush and failed now is stale, a warning, and keeps the
    /// bytes it had. The flush used to stamp every edge fetched before
    /// recording its failure, so both read as warnings; and then wrote the
    /// failed edge's NULL over the stored hash.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_blob_is_an_error_until_it_has_landed_once() {
        let d = tempdir().unwrap();
        let pool = crate::doltlite_raw::open(
            &d.path().join("x.doltlite_db"),
            &[
                "CREATE TABLE IF NOT EXISTS widget_blobs \
                 (id TEXT PRIMARY KEY, owner TEXT, blake3 TEXT)",
                &crate::doltlite_raw::bookkeeping_ddl_for("widget_blobs"),
            ],
        )
        .await
        .unwrap();
        let cas = BlobCas::open(&d.path().join("blobs.sqlite")).await.unwrap();

        let mut acc = CasEdgeAccumulator::new();
        acc.add_failed("w1", "never", "HTTP 500");
        acc.add_fetched("w1", "landed", b"bytes".to_vec(), None, None);
        flush_widget_blobs(&pool, &cas, &acc).await;
        assert_eq!(
            fetch_problem(&pool, "w1#never").await,
            ("error".to_string(), "dropped".to_string())
        );

        let mut acc = CasEdgeAccumulator::new();
        acc.add_failed("w1", "landed", "HTTP 500");
        flush_widget_blobs(&pool, &cas, &acc).await;
        assert_eq!(
            fetch_problem(&pool, "w1#landed").await,
            ("warning".to_string(), "ok".to_string())
        );
        let kept: Option<String> =
            sqlx::query_scalar("SELECT blake3 FROM widget_blobs WHERE id = 'w1#landed'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(
            kept,
            Some(blake3_hex(b"bytes")),
            "a failed read keeps the edge on the bytes it already had"
        );

        cas.close().await;
        pool.close().await;
    }
}
