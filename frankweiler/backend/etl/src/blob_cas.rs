//! Content-addressable blob store + provider-agnostic blob refs.
//!
//! Each raw source ends up with two doltlite files:
//!
//!   `<data_root>/raw/<name>.doltlite_db`        — entities + `blob_refs`
//!   `<data_root>/raw/<name>.blobs.doltlite_db`  — pure CAS (this module)
//!
//! Bytes are keyed by their blake3 hash and stored exactly once in
//! `cas_objects`. The entity db holds a `blob_refs` row per upstream
//! attachment slot, carrying upstream metadata (uuid, original filename,
//! source url) and a nullable hash pointing into the CAS. NULL hash =
//! "we know this attachment exists but haven't fetched bytes yet".
//!
//! Cross-file atomicity is not enforced. CAS inserts that succeed but
//! whose ref-attach fails leave orphan bytes; a GC sweep
//! ([`gc_orphans`]) reconciles. Refs that point at a missing hash are
//! the same shape as today's pre-seeded stub: the next extract retries
//! the fetch.
//!
//! Shared blob_refs DDL lives in [`crate::doltlite_raw::SHARED_DDL`] so
//! every provider's entity db gets it for free.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::Arc;

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow};
use sqlx::{Row, SqlitePool, Transaction};

// ─────────────────────────────────────────────────────────────────────
// Schema
// ─────────────────────────────────────────────────────────────────────

/// Sole table in the per-source blobs database. Pure content-addressed
/// storage: bytes keyed by their blake3, nothing source-specific.
pub const CAS_OBJECTS_DDL: &str = "CREATE TABLE IF NOT EXISTS cas_objects (
    blake3        TEXT PRIMARY KEY,
    byte_len      INTEGER NOT NULL,
    content_type  TEXT NULL,
    bytes         BLOB NOT NULL,
    first_seen_at TEXT NOT NULL,
    CHECK (length(blake3) = 64)
)";

/// Per-source attachment-slot table. Lives in the *entity* db so a
/// dolt diff of a Slack message shows the attachment metadata change
/// inline with the message. PK is the upstream-stable id (or
/// `{owning_id}:{slot}` fallback) — same policy as the old `blobs`
/// table. `blake3` is a logical FK into the sibling `cas_objects`
/// table; NULL means the bytes haven't been fetched yet.
pub const BLOB_REFS_DDL: &str = "CREATE TABLE IF NOT EXISTS blob_refs (
    ref_id        TEXT PRIMARY KEY,
    kind          TEXT NOT NULL,
    owning_id     TEXT NOT NULL,
    slot          TEXT NOT NULL,
    upstream_uuid TEXT NULL,
    upstream_name TEXT NULL,
    source_url    TEXT NULL,
    content_type  TEXT NULL,
    blake3        TEXT NULL,
    CHECK (blake3 IS NULL OR length(blake3) = 64)
)";

pub const BLOB_REFS_BLAKE3_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS blob_refs_blake3 ON blob_refs(blake3)";

pub const BLOB_REFS_OWNING_INDEX_DDL: &str =
    "CREATE INDEX IF NOT EXISTS blob_refs_owning ON blob_refs(owning_id)";

pub const BLOB_REFS_BOOKKEEPING_DDL: &str = "CREATE TABLE IF NOT EXISTS blob_refs_bookkeeping (
    ref_id           TEXT PRIMARY KEY,
    fetched_at       TEXT NULL,
    attempt_count    INTEGER NOT NULL DEFAULT 0,
    last_attempt_at  TEXT NULL,
    last_error       TEXT NULL
)";

// ─────────────────────────────────────────────────────────────────────
// Path helpers
// ─────────────────────────────────────────────────────────────────────

/// Given the entity db path (e.g. `/x/raw/slack.doltlite_db`), return
/// the sibling CAS path `/x/raw/slack.blobs.doltlite_db`.
pub fn cas_path_for(entity_db_path: &Path) -> PathBuf {
    let stem = entity_db_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("blobs");
    let parent = entity_db_path.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!("{stem}.blobs.doltlite_db"))
}

// ─────────────────────────────────────────────────────────────────────
// CAS
// ─────────────────────────────────────────────────────────────────────

/// A row from `cas_objects` — bytes plus the declared content type and
/// length. Hash is implicit (you fetched it by hash).
#[derive(Debug, Clone)]
pub struct CasObject {
    pub blake3: String,
    pub byte_len: i64,
    pub content_type: Option<String>,
    pub bytes: Vec<u8>,
}

/// Per-source CAS handle. Single sqlx pool of size 1, same as every
/// other doltlite store in this codebase.
#[derive(Clone)]
pub struct BlobCas {
    pool: SqlitePool,
}

impl BlobCas {
    pub async fn open(cas_path: &Path) -> Result<Self> {
        if let Some(parent) = cas_path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("create dir {}", parent.display()))?;
        }
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", cas_path.display()))
            .with_context(|| format!("sqlite uri for {}", cas_path.display()))?
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .context("open blob cas pool")?;
        sqlx::query(CAS_OBJECTS_DDL)
            .execute(&pool)
            .await
            .context("apply cas_objects DDL")?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Hash bytes and store them if absent. Returns the lowercase
    /// 64-hex blake3 hash either way. `INSERT OR IGNORE`: identical
    /// bytes from different ref slots collapse to one row.
    pub async fn put(&self, bytes: &[u8], content_type: Option<&str>) -> Result<String> {
        let hash = blake3_hex(bytes);
        let now = chrono::Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT OR IGNORE INTO cas_objects \
             (blake3, byte_len, content_type, bytes, first_seen_at) \
             VALUES (?, ?, ?, ?, ?)",
        )
        .bind(&hash)
        .bind(bytes.len() as i64)
        .bind(content_type)
        .bind(bytes)
        .bind(&now)
        .execute(&self.pool)
        .await
        .context("cas put")?;
        Ok(hash)
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

// ─────────────────────────────────────────────────────────────────────
// blob_refs (live in the entity db; helpers are tx-based)
// ─────────────────────────────────────────────────────────────────────

/// Args for [`pre_seed_ref`]. A struct over a 7-arg fn — providers
/// were already passing most of these one positional at a time and
/// the named form documents itself.
#[derive(Debug, Default, Clone)]
pub struct RefStub<'a> {
    pub ref_id: &'a str,
    pub kind: &'a str,
    pub owning_id: &'a str,
    pub slot: &'a str,
    pub upstream_uuid: Option<&'a str>,
    pub upstream_name: Option<&'a str>,
    pub source_url: Option<&'a str>,
    pub content_type: Option<&'a str>,
}

/// Insert a blob_refs stub before the bytes are fetched, plus a
/// matching bookkeeping row. `INSERT OR IGNORE` on both so a re-list
/// over an already-attached ref doesn't clobber its hash or its
/// fetch history.
pub async fn pre_seed_ref(
    tx: &mut Transaction<'_, sqlx::Sqlite>,
    stub: &RefStub<'_>,
) -> Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO blob_refs \
         (ref_id, kind, owning_id, slot, upstream_uuid, upstream_name, source_url, content_type) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(stub.ref_id)
    .bind(stub.kind)
    .bind(stub.owning_id)
    .bind(stub.slot)
    .bind(stub.upstream_uuid)
    .bind(stub.upstream_name)
    .bind(stub.source_url)
    .bind(stub.content_type)
    .execute(&mut **tx)
    .await
    .with_context(|| format!("pre_seed_ref {}", stub.ref_id))?;
    sqlx::query("INSERT OR IGNORE INTO blob_refs_bookkeeping (ref_id) VALUES (?)")
        .bind(stub.ref_id)
        .execute(&mut **tx)
        .await
        .with_context(|| format!("pre_seed_ref bookkeeping {}", stub.ref_id))?;
    Ok(())
}

/// Attach a hash + content_type to an existing ref, OR create the ref
/// row if it doesn't exist yet. Mirrors today's `upsert_blob_bytes`
/// behavior where the stub is optional. Bumps the bookkeeping success
/// counters.
pub async fn attach_hash(
    tx: &mut Transaction<'_, sqlx::Sqlite>,
    stub: &RefStub<'_>,
    blake3_hash: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO blob_refs \
         (ref_id, kind, owning_id, slot, upstream_uuid, upstream_name, source_url, content_type, blake3) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?) \
         ON CONFLICT(ref_id) DO UPDATE SET \
            kind          = excluded.kind, \
            owning_id     = excluded.owning_id, \
            slot          = excluded.slot, \
            upstream_uuid = COALESCE(excluded.upstream_uuid, blob_refs.upstream_uuid), \
            upstream_name = COALESCE(excluded.upstream_name, blob_refs.upstream_name), \
            source_url    = COALESCE(excluded.source_url,    blob_refs.source_url), \
            content_type  = COALESCE(excluded.content_type,  blob_refs.content_type), \
            blake3        = excluded.blake3",
    )
    .bind(stub.ref_id)
    .bind(stub.kind)
    .bind(stub.owning_id)
    .bind(stub.slot)
    .bind(stub.upstream_uuid)
    .bind(stub.upstream_name)
    .bind(stub.source_url)
    .bind(stub.content_type)
    .bind(blake3_hash)
    .execute(&mut **tx)
    .await
    .with_context(|| format!("attach_hash {}", stub.ref_id))?;
    crate::doltlite_raw::record_object_attempt(tx, "blob_refs", stub.ref_id, None).await
}

/// Record a failed fetch attempt. Creates a stub row if needed.
pub async fn record_ref_error(
    tx: &mut Transaction<'_, sqlx::Sqlite>,
    ref_id: &str,
    owning_id: &str,
    slot: &str,
    err: &str,
) -> Result<()> {
    sqlx::query(
        "INSERT OR IGNORE INTO blob_refs (ref_id, kind, owning_id, slot) \
         VALUES (?, 'unknown', ?, ?)",
    )
    .bind(ref_id)
    .bind(owning_id)
    .bind(slot)
    .execute(&mut **tx)
    .await
    .with_context(|| format!("record_ref_error data {ref_id}"))?;
    crate::doltlite_raw::record_object_attempt(tx, "blob_refs", ref_id, Some(err)).await
}

/// True iff the ref already has a hash attached (i.e. bytes are in
/// the CAS). Cheap short-circuit for skip-already-fetched paths.
pub async fn ref_has_hash(pool: &SqlitePool, ref_id: &str) -> Result<bool> {
    let row =
        sqlx::query("SELECT 1 FROM blob_refs WHERE ref_id = ? AND blake3 IS NOT NULL LIMIT 1")
            .bind(ref_id)
            .fetch_optional(pool)
            .await
            .context("ref_has_hash")?;
    Ok(row.is_some())
}

// ─────────────────────────────────────────────────────────────────────
// Combined extract-side helper: write bytes + attach + bookkeeping
// ─────────────────────────────────────────────────────────────────────

/// One-shot "we fetched the bytes — persist them everywhere".
/// Performs three writes in this order:
///   1. `cas.put(bytes, ct)` → returns the hash.
///   2. `attach_hash(ref_id, hash)` against the entity db (with
///      bookkeeping success).
///
/// Steps 1 and 2 span two doltlite files; if step 1 succeeds and step
/// 2 fails the CAS gains an orphan row that [`gc_orphans`] sweeps
/// later. If step 2 succeeds with a hash that never made it into the
/// CAS, the next render will see a `read_by_ref_id` miss and the
/// caller's existing "missing bytes" handling fires.
pub async fn store_bytes(
    entity_pool: &SqlitePool,
    cas: &BlobCas,
    stub: &RefStub<'_>,
    bytes: &[u8],
) -> Result<String> {
    let hash = cas.put(bytes, stub.content_type).await?;
    let mut tx = entity_pool.begin().await.context("begin attach_hash tx")?;
    attach_hash(&mut tx, stub, &hash).await?;
    tx.commit().await.context("commit attach_hash tx")?;
    Ok(hash)
}

// ─────────────────────────────────────────────────────────────────────
// Read side — BlobReader trait + impls
// ─────────────────────────────────────────────────────────────────────

/// One blob materialized for the renderer: enough metadata to write a
/// file and link to it.
#[derive(Debug, Clone)]
pub struct BlobView {
    pub ref_id: String,
    pub owning_id: String,
    pub slot: String,
    pub blake3: String,
    pub content_type: Option<String>,
    pub upstream_name: Option<String>,
    pub source_url: Option<String>,
    pub bytes: Vec<u8>,
}

impl BlobView {
    /// Filename the renderer should write the bytes under. Hash-first
    /// so the path is opaque and collision-safe regardless of what the
    /// upstream filename looked like. Extension comes from
    /// `content_type` when known.
    pub fn rendered_filename(&self) -> String {
        let ext = extension_for_content_type(self.content_type.as_deref())
            .or_else(|| extension_from_upstream_name(self.upstream_name.as_deref()));
        let short = &self.blake3[..16.min(self.blake3.len())];
        match ext {
            Some(e) => format!("{short}.{e}"),
            None => short.to_string(),
        }
    }

    /// Markdown link `[display](blobs/<filename>)`. `display` falls
    /// back to the upstream name, then the short hash.
    pub fn markdown_link(&self, display: Option<&str>) -> String {
        let fname = self.rendered_filename();
        let text = display
            .or(self.upstream_name.as_deref())
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| fname.clone());
        format!("[{text}](blobs/{fname})")
    }
}

/// Sync trait so renderers (which run under a sync translate shape) can
/// stream blob bytes one at a time without threading async-ness through
/// every renderer.
pub trait BlobReader: Send + Sync {
    fn read_by_ref_id(&self, ref_id: &str) -> Result<Option<BlobView>>;
    fn read_by_owner(&self, owning_id: &str) -> Result<Option<BlobView>>;
    fn read_by_hash(&self, blake3_hash: &str) -> Result<Option<BlobView>>;
}

/// Sqlite-backed reader: takes both the entity pool (for ref lookups)
/// and the CAS pool (for bytes). Bridges async sqlx into the sync
/// trait via `block_in_place` + the current tokio Handle — translate
/// callers always live under `#[tokio::main]`.
pub struct SqliteBlobReader {
    refs_pool: SqlitePool,
    cas_pool: SqlitePool,
}

impl SqliteBlobReader {
    pub fn new(refs_pool: SqlitePool, cas_pool: SqlitePool) -> Self {
        Self {
            refs_pool,
            cas_pool,
        }
    }

    pub fn into_handle(self) -> Arc<dyn BlobReader> {
        Arc::new(self)
    }

    fn block_on<F: std::future::Future>(&self, fut: F) -> F::Output {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
    }

    async fn ref_row(&self, ref_id: &str) -> Result<Option<RefRow>> {
        let row = sqlx::query(
            "SELECT ref_id, owning_id, slot, blake3, content_type, upstream_name, source_url \
             FROM blob_refs WHERE ref_id = ? AND blake3 IS NOT NULL",
        )
        .bind(ref_id)
        .fetch_optional(&self.refs_pool)
        .await
        .with_context(|| format!("ref_row {ref_id}"))?;
        Ok(row.map(row_to_ref))
    }

    async fn ref_row_for_owner(&self, owning_id: &str) -> Result<Option<RefRow>> {
        let row = sqlx::query(
            "SELECT ref_id, owning_id, slot, blake3, content_type, upstream_name, source_url \
             FROM blob_refs WHERE owning_id = ? AND blake3 IS NOT NULL \
             ORDER BY ref_id DESC LIMIT 1",
        )
        .bind(owning_id)
        .fetch_optional(&self.refs_pool)
        .await
        .with_context(|| format!("ref_row_for_owner {owning_id}"))?;
        Ok(row.map(row_to_ref))
    }

    async fn assemble(&self, r: RefRow) -> Result<Option<BlobView>> {
        let row = sqlx::query("SELECT bytes, content_type FROM cas_objects WHERE blake3 = ?")
            .bind(&r.blake3)
            .fetch_optional(&self.cas_pool)
            .await
            .with_context(|| format!("cas bytes {}", r.blake3))?;
        let Some(row) = row else { return Ok(None) };
        let bytes: Vec<u8> = row.try_get("bytes").unwrap_or_default();
        let cas_ct: Option<String> = row.try_get("content_type").ok();
        Ok(Some(BlobView {
            ref_id: r.ref_id,
            owning_id: r.owning_id,
            slot: r.slot,
            blake3: r.blake3,
            content_type: r.content_type.or(cas_ct),
            upstream_name: r.upstream_name,
            source_url: r.source_url,
            bytes,
        }))
    }
}

struct RefRow {
    ref_id: String,
    owning_id: String,
    slot: String,
    blake3: String,
    content_type: Option<String>,
    upstream_name: Option<String>,
    source_url: Option<String>,
}

fn row_to_ref(r: SqliteRow) -> RefRow {
    RefRow {
        ref_id: r.try_get("ref_id").unwrap_or_default(),
        owning_id: r.try_get("owning_id").unwrap_or_default(),
        slot: r.try_get("slot").unwrap_or_default(),
        blake3: r.try_get("blake3").unwrap_or_default(),
        content_type: r.try_get("content_type").ok(),
        upstream_name: r.try_get("upstream_name").ok(),
        source_url: r.try_get("source_url").ok(),
    }
}

impl BlobReader for SqliteBlobReader {
    fn read_by_ref_id(&self, ref_id: &str) -> Result<Option<BlobView>> {
        self.block_on(async {
            match self.ref_row(ref_id).await? {
                Some(r) => self.assemble(r).await,
                None => Ok(None),
            }
        })
    }

    fn read_by_owner(&self, owning_id: &str) -> Result<Option<BlobView>> {
        self.block_on(async {
            match self.ref_row_for_owner(owning_id).await? {
                Some(r) => self.assemble(r).await,
                None => Ok(None),
            }
        })
    }

    fn read_by_hash(&self, blake3_hash: &str) -> Result<Option<BlobView>> {
        self.block_on(async {
            let row = sqlx::query("SELECT bytes, content_type FROM cas_objects WHERE blake3 = ?")
                .bind(blake3_hash)
                .fetch_optional(&self.cas_pool)
                .await
                .with_context(|| format!("cas bytes {blake3_hash}"))?;
            let Some(row) = row else { return Ok(None) };
            let bytes: Vec<u8> = row.try_get("bytes").unwrap_or_default();
            let content_type: Option<String> = row.try_get("content_type").ok();
            Ok(Some(BlobView {
                ref_id: String::new(),
                owning_id: String::new(),
                slot: String::new(),
                blake3: blake3_hash.to_string(),
                content_type,
                upstream_name: None,
                source_url: None,
                bytes,
            }))
        })
    }
}

/// In-memory reader for tests. Holds `BlobView`s keyed by ref_id; the
/// owner index walks values like the sqlite impl's "lexically-last
/// ref_id wins" rule.
#[derive(Default)]
pub struct InMemoryBlobReader {
    by_ref: HashMap<String, BlobView>,
}

impl InMemoryBlobReader {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, view: BlobView) {
        self.by_ref.insert(view.ref_id.clone(), view);
    }

    pub fn into_handle(self) -> Arc<dyn BlobReader> {
        Arc::new(self)
    }

    pub fn empty_handle() -> Arc<dyn BlobReader> {
        Arc::new(Self::new())
    }
}

impl BlobReader for InMemoryBlobReader {
    fn read_by_ref_id(&self, ref_id: &str) -> Result<Option<BlobView>> {
        Ok(self.by_ref.get(ref_id).cloned())
    }

    fn read_by_owner(&self, owning_id: &str) -> Result<Option<BlobView>> {
        Ok(self
            .by_ref
            .values()
            .filter(|v| v.owning_id == owning_id)
            .max_by(|a, b| a.ref_id.cmp(&b.ref_id))
            .cloned())
    }

    fn read_by_hash(&self, blake3_hash: &str) -> Result<Option<BlobView>> {
        Ok(self
            .by_ref
            .values()
            .find(|v| v.blake3 == blake3_hash)
            .cloned())
    }
}

// ─────────────────────────────────────────────────────────────────────
// Render-side helper: materialize a blob to disk + return relative path
// ─────────────────────────────────────────────────────────────────────

/// Write a blob's bytes into `blobs_dir/<short-b3>.<ext>` and return
/// the relative `blobs/<filename>` path the renderer should embed in
/// markdown. Returns `Ok(None)` if the ref is missing or has no
/// attached bytes — the caller decides whether to elide the link or
/// emit a placeholder.
pub fn materialize_to_disk(
    reader: &dyn BlobReader,
    ref_id: &str,
    blobs_dir: &Path,
) -> Result<Option<(BlobView, PathBuf, String)>> {
    let Some(view) = reader.read_by_ref_id(ref_id)? else {
        return Ok(None);
    };
    let fname = view.rendered_filename();
    std::fs::create_dir_all(blobs_dir)
        .with_context(|| format!("create {}", blobs_dir.display()))?;
    let abs = blobs_dir.join(&fname);
    std::fs::write(&abs, &view.bytes).with_context(|| format!("write blob {}", abs.display()))?;
    let rel = format!("blobs/{fname}");
    Ok(Some((view, abs, rel)))
}

// ─────────────────────────────────────────────────────────────────────
// content-type → extension
// ─────────────────────────────────────────────────────────────────────

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

/// Pull the trailing `.ext` off an upstream filename if it has one.
pub fn extension_from_upstream_name(name: Option<&str>) -> Option<String> {
    let name = name?;
    let (_, ext) = name.rsplit_once('.')?;
    if ext.is_empty() || ext.len() > 8 || !ext.chars().all(|c| c.is_ascii_alphanumeric()) {
        return None;
    }
    Some(ext.to_ascii_lowercase())
}

// ─────────────────────────────────────────────────────────────────────
// Maintenance
// ─────────────────────────────────────────────────────────────────────

/// Hashes referenced from any blob_refs row. Pass one tuple per
/// entity db that points at this CAS file; the union is the GC root
/// set. Single-writer regime: callers serialize this against extracts.
pub async fn referenced_hashes(refs_pools: &[&SqlitePool]) -> Result<Vec<String>> {
    let mut out: Vec<String> = Vec::new();
    for p in refs_pools {
        let rows = sqlx::query("SELECT DISTINCT blake3 FROM blob_refs WHERE blake3 IS NOT NULL")
            .fetch_all(*p)
            .await
            .context("select referenced hashes")?;
        for r in rows {
            if let Ok(h) = r.try_get::<String, _>("blake3") {
                out.push(h);
            }
        }
    }
    Ok(out)
}

/// Delete CAS rows whose hash isn't in `keep`. Returns the deletion
/// count. Caller is responsible for having gathered every referencing
/// entity db's hashes before calling.
pub async fn gc_orphans(cas: &BlobCas, keep: &[String]) -> Result<u64> {
    let mut tx = cas.pool().begin().await.context("begin gc tx")?;
    sqlx::query("CREATE TEMP TABLE _keep (blake3 TEXT PRIMARY KEY)")
        .execute(&mut *tx)
        .await
        .context("create temp _keep")?;
    for h in keep {
        sqlx::query("INSERT OR IGNORE INTO _keep (blake3) VALUES (?)")
            .bind(h)
            .execute(&mut *tx)
            .await
            .context("insert _keep")?;
    }
    let res = sqlx::query("DELETE FROM cas_objects WHERE blake3 NOT IN (SELECT blake3 FROM _keep)")
        .execute(&mut *tx)
        .await
        .context("delete orphans")?;
    sqlx::query("DROP TABLE _keep")
        .execute(&mut *tx)
        .await
        .context("drop temp _keep")?;
    tx.commit().await.context("commit gc tx")?;
    Ok(res.rows_affected())
}

// ─────────────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test(flavor = "multi_thread")]
    async fn cas_put_is_idempotent() {
        let d = tempdir().unwrap();
        let cas = BlobCas::open(&d.path().join("x.blobs.doltlite_db"))
            .await
            .unwrap();
        let h1 = cas.put(b"hello", Some("text/plain")).await.unwrap();
        let h2 = cas.put(b"hello", Some("text/plain")).await.unwrap();
        assert_eq!(h1, h2);
        let got = cas.get(&h1).await.unwrap().unwrap();
        assert_eq!(got.bytes, b"hello");
    }

    #[test]
    fn rendered_filename_uses_content_type() {
        let v = BlobView {
            ref_id: "r".into(),
            owning_id: "o".into(),
            slot: "s".into(),
            blake3: "a".repeat(64),
            content_type: Some("image/png".into()),
            upstream_name: Some("My Photo.PNG".into()),
            source_url: None,
            bytes: vec![],
        };
        assert_eq!(v.rendered_filename(), "aaaaaaaaaaaaaaaa.png");
    }

    #[test]
    fn rendered_filename_falls_back_to_upstream_ext() {
        let v = BlobView {
            ref_id: "r".into(),
            owning_id: "o".into(),
            slot: "s".into(),
            blake3: "b".repeat(64),
            content_type: None,
            upstream_name: Some("report.pdf".into()),
            source_url: None,
            bytes: vec![],
        };
        assert_eq!(v.rendered_filename(), "bbbbbbbbbbbbbbbb.pdf");
    }

    #[test]
    fn markdown_link_prefers_upstream_name() {
        let v = BlobView {
            ref_id: "r".into(),
            owning_id: "o".into(),
            slot: "s".into(),
            blake3: "c".repeat(64),
            content_type: Some("application/pdf".into()),
            upstream_name: Some("Q3 budget.pdf".into()),
            source_url: None,
            bytes: vec![],
        };
        assert_eq!(
            v.markdown_link(None),
            "[Q3 budget.pdf](blobs/cccccccccccccccc.pdf)"
        );
    }

    #[test]
    fn cas_path_for_swaps_extension() {
        let p = Path::new("/tmp/raw/slack.doltlite_db");
        assert_eq!(
            cas_path_for(p),
            PathBuf::from("/tmp/raw/slack.blobs.doltlite_db")
        );
    }
}
