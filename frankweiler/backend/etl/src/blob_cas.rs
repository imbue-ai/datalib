//! Content-addressable blob store + per-bucket attachment bundle.
//!
//! Each raw source owns a directory holding two doltlite files:
//!
//!   `<data_root>/raw/<name>/entities.doltlite_db`  — entities + per-provider CAS edge table
//!   `<data_root>/raw/<name>/blobs.doltlite_db`     — pure CAS (this module)
//!
//! Bytes are keyed by their blake3 hash and stored exactly once in
//! `cas_objects`. Each provider declares its own `(<owning>, <ref>,
//! blake3)` edge table via the [`CasEdgeRow`] derive — see
//! `slack_attachments`, `wa_media_files`, `notion_image_attachments`,
//! etc. for the per-provider names. The render side consumes
//! attachments per bucket via [`BlobBundle::load`], which joins the
//! edge table to `cas_objects` to assemble one bag of bytes per
//! rendered doc.
//!
//! The legacy shared `blob_refs` table + its `RefStub` / `store_bytes`
//! / `attach_hash` write API was retired once every provider moved
//! to the per-provider edge shape; see git history for the old API.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::{Context, Result};
use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions, SqliteRow};
use sqlx::{Row, SqlitePool};

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

// ─────────────────────────────────────────────────────────────────────
// Path helpers
// ─────────────────────────────────────────────────────────────────────

/// Given the entity db path (e.g. `/x/raw/slack/entities.doltlite_db`),
/// return the sibling CAS path `/x/raw/slack/blobs.doltlite_db`. Both
/// files live inside the per-source directory; the filename comes from
/// [`crate::raw_layout::BLOBS_DB`].
pub fn cas_path_for(entity_db_path: &Path) -> PathBuf {
    let parent = entity_db_path.parent().unwrap_or_else(|| Path::new("."));
    crate::raw_layout::blobs_db(parent)
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

/// Per-source CAS handle. Single sqlx pool of size 1, same as every
/// other doltlite store in this codebase.
#[derive(Clone, Debug)]
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
        let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
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
        crate::extract_metrics::record_upserts("cas_objects", 1);
        Ok(hash)
    }

    /// Bulk-insert pre-hashed bytes in a single transaction, using
    /// chunked multi-row `INSERT OR IGNORE` (one prolly-tree manifest
    /// mutation per chunk's `COMMIT` instead of one per blob).
    ///
    /// The caller must precompute the blake3 hex hash of each item
    /// (use [`blake3_hex`]) — `put_many` does not re-hash.
    ///
    /// See `docs/dev/data_architecture_ingestion.md` § "Bulk-upsert as
    /// the standard write path" for why CAS writes share the same
    /// batching shape as entity writes. No-op if `items` is empty.
    pub async fn put_many(&self, items: &[CasInsert<'_>]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        let mut tx = self.pool.begin().await.context("begin cas put_many tx")?;
        for chunk in items.chunks(crate::bulk::SQL_CHUNK) {
            let mut sql = String::from(
                "INSERT OR IGNORE INTO cas_objects \
                 (blake3, byte_len, content_type, bytes, first_seen_at) VALUES ",
            );
            crate::bulk::push_placeholders(&mut sql, chunk.len(), 5);
            let mut q = sqlx::query(&sql);
            for it in chunk {
                q = q
                    .bind(it.blake3)
                    .bind(it.bytes.len() as i64)
                    .bind(it.content_type)
                    .bind(it.bytes)
                    .bind(&now);
            }
            q.execute(&mut *tx)
                .await
                .context("bulk insert cas_objects")?;
        }
        tx.commit().await.context("commit cas put_many tx")?;
        // Tally CAS writes against the current source's extract metrics
        // (no-op outside an extract scope). Counts attempts; some are
        // INSERT-OR-IGNORE dupes, so rows_after - rows_before is the
        // real net-new figure.
        crate::extract_metrics::record_upserts("cas_objects", items.len());
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

// ─────────────────────────────────────────────────────────────────────
// Read side — every provider now uses [`BlobBundle`] (below). The
// retired `BlobView` / `BlobReader` / `SqliteBlobReader` /
// `InMemoryBlobReader` / `materialize_to_disk` / `materialize_refs` /
// `attachment_md` surface was deleted with the notion port (the last
// consumer); see git history if you need its old shape.
// ─────────────────────────────────────────────────────────────────────

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
// BlobBundle — per-doc unit of attachment data, read + write
// ─────────────────────────────────────────────────────────────────────

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
    /// Stable on-disk filename: `<short-blake3>.<ext>`. Extension comes
    /// from `content_type` when known, else from the upstream filename.
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

/// One fetched-but-not-yet-flushed entry on the extract side, exposed
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
///
/// - **Extract** builds an empty bundle, calls [`Self::add`] as bytes
///   come in (and [`Self::add_error`] when a fetch fails), then asks
///   the per-provider flush code to drain it via
///   [`Self::cas_inserts`] (→ [`BlobCas::put_many`]),
///   [`Self::fetched_refs`] (→ the provider's edge table), and
///   [`Self::errors`] (→ `record_object_error`).
///
/// - **Parse** calls [`Self::load`] for one doc's set of `ref_id`s and
///   attaches the resulting bundle to the parsed bucket. Two SQL
///   queries total — one for the per-provider `ref_id → blake3 +
///   metadata` projection, one for `cas_objects` bytes — regardless of
///   how many attachments the doc has.
///
/// - **Render** consumes the bundle synchronously via [`Self::get`],
///   [`Self::materialize_to_dir`], and [`Self::markdown_link`]. No SQL,
///   no `tokio::task::block_in_place`, no dyn-dispatched blob reader
///   — render is a pure transformer over an already-loaded bag of
///   bytes.
///
/// Same conceptual shape both ends. The "blob read" and "blob write"
/// operations are mirror images, and the bundle is the common
/// vocabulary.
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

    // ── extract side ─────────────────────────────────────────────────

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

    /// Record one failed fetch. The flush code routes these through
    /// `record_object_error` on the provider's edge-table bookkeeping
    /// sidecar.
    pub fn add_error(&mut self, ref_id: impl Into<String>, error: impl Into<String>) {
        self.errors.push((ref_id.into(), error.into()));
    }

    /// Borrow-friendly view for the per-provider flush code that has
    /// to call [`BlobCas::put_many`].
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

    /// Bulk-load one doc's attachments. Two SQL queries, regardless of
    /// how many attachments:
    ///
    /// 1. **Projection** (per-provider). Caller supplies a SQL string
    ///    with **exactly one `{placeholders}` substring** where the
    ///    `?, ?, ...` IN-list should land. Must `SELECT ref_id, blake3,
    ///    content_type, upstream_name` (any of the last two may be
    ///    `NULL`). Example:
    ///
    ///    ```text
    ///    SELECT file_id AS ref_id, blake3,
    ///           NULL AS content_type, NULL AS upstream_name
    ///      FROM chatgpt_attachments
    ///     WHERE file_id IN ({placeholders}) AND blake3 IS NOT NULL
    ///    ```
    ///
    /// 2. **CAS bytes**: `SELECT blake3, bytes, content_type FROM
    ///    cas_objects WHERE blake3 IN (?, ...)`. The provider's
    ///    `content_type` wins over `cas_objects.content_type` when
    ///    both are present.
    ///
    /// Returns an empty bundle if `ref_ids` is empty. Refs that the
    /// projection didn't surface (no row, or `blake3 IS NULL`) are
    /// silently dropped — render then emits the
    /// "attachment not yet fetched" placeholder via
    /// [`Self::markdown_link`].
    pub async fn load(
        refs_pool: &SqlitePool,
        cas_pool: &SqlitePool,
        projection_sql_template: &str,
        ref_ids: &[&str],
    ) -> Result<Self> {
        if ref_ids.is_empty() {
            return Ok(Self::new());
        }
        // Stage 1: ref_id → (blake3, content_type, upstream_name).
        // The template may use `{placeholders}` more than once (e.g.
        // email's UNION ALL over `emails` and `email_attachments`
        // wants the same IN-list twice); we bind the ref_ids once
        // per occurrence so the binding order matches the SQL.
        let placeholders = std::iter::repeat_n("?", ref_ids.len())
            .collect::<Vec<_>>()
            .join(",");
        let occurrences = projection_sql_template.matches("{placeholders}").count();
        let sql = projection_sql_template.replace("{placeholders}", &placeholders);
        let mut q = sqlx::query(&sql);
        for _ in 0..occurrences {
            for r in ref_ids {
                q = q.bind(*r);
            }
        }
        let rows = q
            .fetch_all(refs_pool)
            .await
            .context("BlobBundle::load projection")?;
        if rows.is_empty() {
            return Ok(Self::new());
        }
        // Build a temporary (blake3 → entries-pointing-at-it) map so a
        // single CAS query covers the whole set even if multiple
        // ref_ids dedupe to one blake3.
        struct PendingEntry {
            ref_id: String,
            content_type: Option<String>,
            upstream_name: Option<String>,
        }
        let mut pending_by_blake3: HashMap<String, Vec<PendingEntry>> = HashMap::new();
        let mut blake3_set: Vec<String> = Vec::with_capacity(rows.len());
        for r in &rows {
            let Ok(ref_id) = r.try_get::<String, _>("ref_id") else {
                continue;
            };
            let Ok(blake3) = r.try_get::<String, _>("blake3") else {
                continue;
            };
            let content_type: Option<String> = r.try_get("content_type").ok().flatten();
            let upstream_name: Option<String> = r.try_get("upstream_name").ok().flatten();
            if !pending_by_blake3.contains_key(&blake3) {
                blake3_set.push(blake3.clone());
            }
            pending_by_blake3
                .entry(blake3)
                .or_default()
                .push(PendingEntry {
                    ref_id,
                    content_type,
                    upstream_name,
                });
        }
        if pending_by_blake3.is_empty() {
            return Ok(Self::new());
        }
        // Stage 2: cas_objects bytes for every blake3 we found.
        let cas_placeholders = std::iter::repeat_n("?", blake3_set.len())
            .collect::<Vec<_>>()
            .join(",");
        let cas_sql = format!(
            "SELECT blake3, bytes, content_type \
               FROM cas_objects WHERE blake3 IN ({cas_placeholders})"
        );
        let mut cq = sqlx::query(&cas_sql);
        for h in &blake3_set {
            cq = cq.bind(h);
        }
        let cas_rows = cq
            .fetch_all(cas_pool)
            .await
            .context("BlobBundle::load cas_objects")?;
        let mut bundle = Self::new();
        for cr in &cas_rows {
            let Ok(blake3) = cr.try_get::<String, _>("blake3") else {
                continue;
            };
            let bytes: Vec<u8> = cr.try_get("bytes").unwrap_or_default();
            let cas_ct: Option<String> = cr.try_get("content_type").ok().flatten();
            let Some(entries) = pending_by_blake3.remove(&blake3) else {
                continue;
            };
            for entry in entries {
                bundle.by_ref.insert(
                    entry.ref_id,
                    Blob {
                        blake3: blake3.clone(),
                        bytes: bytes.clone(),
                        content_type: entry.content_type.or_else(|| cas_ct.clone()),
                        upstream_name: entry.upstream_name,
                    },
                );
            }
        }
        Ok(bundle)
    }

    // ── render side (sync) ───────────────────────────────────────────

    /// Write every blob's bytes into `blobs_dir/<rendered_filename>`.
    /// Skips a write when the target file already exists with the
    /// expected size, so re-running render against the same bundle is
    /// idempotent.
    pub fn materialize_to_dir(&self, blobs_dir: &Path) -> std::io::Result<()> {
        if self.by_ref.is_empty() {
            return Ok(());
        }
        std::fs::create_dir_all(blobs_dir)?;
        for blob in self.by_ref.values() {
            let fname = blob.rendered_filename();
            let abs = blobs_dir.join(&fname);
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
        let display_clean = display.unwrap_or("").replace(']', "");
        let alt = if display_clean.is_empty() {
            blob.rendered_filename()
        } else {
            display_clean
        };
        let link = format!("blobs/{}", blob.rendered_filename());
        if is_image {
            format!("![{alt}]({link})")
        } else {
            format!("[\\[file\\] {alt}]({link})")
        }
    }
}

// ─────────────────────────────────────────────────────────────────────
// Per-provider CAS-edge tables — shared shape
// ─────────────────────────────────────────────────────────────────────

/// The shape every per-provider CAS edge table follows. One row per
/// `(owning_id, ref_id)` pair, recording the CAS `blake3` for the
/// bytes that the upstream's `ref_id` resolved to.
///
/// Implementors are row structs with **exactly four fields, in this
/// order**:
///
/// ```ignore
/// #[derive(CasEdgeRow)]
/// #[cas_edge_row(table = "slack_attachments")]
/// pub struct SlackAttachmentRow {
///     pub id: String,           // synth "{owning_id}#{ref_id}"
///     pub message_uuid: String, // owning-entity FK ← OWNING_COLUMN
///     pub file_id: String,      // upstream ref      ← REF_COLUMN
///     pub blake3: Option<String>,
/// }
/// ```
///
/// The proc-macro derive (`frankweiler_etl_macros::CasEdgeRow`) reads
/// the second and third fields' identifiers and emits
/// [`Self::OWNING_COLUMN`] / [`Self::REF_COLUMN`] accordingly, plus
/// the [`crate::bulk::BulkUpsertable`] impl. Default trait methods
/// then synthesize the `CREATE TABLE` + two index DDLs and the
/// `"{owning_id}#{ref_id}"` PK recipe, so each provider's
/// `schema_raw.rs` is just the four-field struct + the attribute.
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

    /// Index on the owning-FK column. Supports "load every edge for
    /// this owner" queries (per-bucket attachment loads on the
    /// render side).
    fn by_owning_index_ddl() -> String {
        format!(
            "CREATE INDEX IF NOT EXISTS {table}_by_{owning} ON {table}({owning})",
            table = Self::TABLE,
            owning = Self::OWNING_COLUMN,
        )
    }

    /// Index on `(ref_column, blake3)` — supports the skip-check
    /// "have we ever stored this ref's bytes" without a full scan,
    /// and the per-thread `BlobBundle::load` projection's
    /// `WHERE ref_id IN (…) AND blake3 IS NOT NULL`.
    fn by_ref_index_ddl() -> String {
        format!(
            "CREATE INDEX IF NOT EXISTS {table}_by_{ref_c} ON {table}({ref_c}, blake3)",
            table = Self::TABLE,
            ref_c = Self::REF_COLUMN,
        )
    }

    /// Convenience: every entry in [`Self::all_ddl`] in one slice,
    /// ready to splice into a provider's `full_ddl()` composer.
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
}

// ─────────────────────────────────────────────────────────────────────
// Per-provider CAS-edge index loader
// ─────────────────────────────────────────────────────────────────────

/// Snapshot a per-provider CAS edge table as a `(ref_id → blake3)`
/// in-memory map. Loaded once at the start of `fetch()` so the
/// per-file "have we got these bytes yet?" check is a HashMap hit
/// instead of a SQLite round trip queued behind preceding multi-MB
/// CAS commits on a single-connection doltlite pool.
///
/// `table` is the per-provider edge table (`chatgpt_attachments`,
/// `anthropic_attachments`, `slack_attachments`); `ref_id_column` is
/// the column carrying the upstream id (`file_id`, `file_uuid`).
/// Many edge rows can share the same `ref_id` (different owning
/// rows); the HashMap collapses duplicates and keeps the first
/// non-null `blake3` we see — they should all agree, since one
/// `ref_id` ↔ one immutable set of bytes ↔ one `blake3`.
///
/// The caller's `fetch()` keeps the map up to date as it goes:
/// each successful download inserts the new (ref_id, blake3) so
/// later files in the same run hit the cache without re-fetching.
pub async fn load_blake3_index(
    pool: &SqlitePool,
    table: &str,
    ref_id_column: &str,
) -> Result<HashMap<String, String>> {
    let sql = format!(
        "SELECT {ref_id_column} AS ref_id, blake3 FROM {table} \
          WHERE blake3 IS NOT NULL"
    );
    let rows = sqlx::query(&sql)
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

// ─────────────────────────────────────────────────────────────────────
// CAS-edge accumulator
// ─────────────────────────────────────────────────────────────────────

/// Per-bucket attachment-fetch accumulator. Every per-provider
/// extract walks an upstream bucket (a conversation, a thread, a
/// channel of messages) and decides per file: did we just fetch
/// bytes, did we discover bytes were already in the CAS, or did
/// the fetch fail? This struct collects those outcomes, then
/// [`Self::flush`] hands them to [`flush_cas_edges`] via a row
/// builder the caller supplies.
///
/// **Three add paths:**
///
///   - [`Self::add_fetched`] — we just downloaded the bytes. Lands in
///     the [`BlobBundle`] so the end-of-bucket CAS write picks them
///     up; the edge row will be stamped with the bundle-derived
///     blake3.
///   - [`Self::add_known`] — the bytes were already in the CAS from a
///     prior sync (or an earlier file in this run hit the same
///     ref_id). Caller passes the looked-up blake3 so the new edge
///     row carries the actual hash.
///   - [`Self::add_failed`] — the fetch errored. Edge row gets
///     `blake3 = NULL` and an error stamp lands on the bookkeeping
///     sidecar.
///
/// One row per `(owning_id, ref_id)` pair — same pair seen twice
/// is a no-op after the first add. End-of-bucket
/// [`Self::flush`] resolves each edge's blake3 (fetched → from
/// bundle, known → from the looked-up map, failed → `None`) and
/// delegates to [`flush_cas_edges`].
pub struct CasEdgeAccumulator {
    bundle: BlobBundle,
    edges: Vec<EdgePending>,
    errors: Vec<(String, String)>,
    seen: std::collections::HashSet<(String, String)>,
    known_blake3: HashMap<String, String>,
}

struct EdgePending {
    owning_id: String,
    ref_id: String,
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

    /// Record `(owning, ref)` edge with freshly-downloaded bytes.
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

    /// Record `(owning, ref)` edge whose bytes were already in CAS
    /// from a prior run. `blake3` is the looked-up hex hash that the
    /// new edge row will carry.
    pub fn add_known(&mut self, owning_id: &str, ref_id: &str, blake3: String) {
        self.push_edge(owning_id, ref_id);
        self.known_blake3
            .entry(ref_id.to_string())
            .or_insert(blake3);
    }

    /// Record `(owning, ref)` edge whose fetch errored. The edge row
    /// gets `blake3 = NULL`; `err` lands on the bookkeeping sidecar
    /// via `record_object_attempt`.
    pub fn add_failed(&mut self, owning_id: &str, ref_id: &str, err: impl Into<String>) {
        self.push_edge(owning_id, ref_id);
        self.errors.push((ref_id.to_string(), err.into()));
        self.bundle.add_error(ref_id, "fetch failed");
    }

    /// End-of-bucket flush. Builds the per-`(owning, ref)` edge rows
    /// via `build_row` and delegates to [`flush_cas_edges`]: CAS
    /// `put_many` → bulk UPSERT edge rows → bookkeeping error stamps.
    ///
    /// `build_row` receives `(owning_id, ref_id, blake3)` and returns
    /// the provider's row type. `blake3` is resolved per edge: the
    /// bundle's `fetched_refs` for new fetches, [`Self::add_known`]'s
    /// recorded hash for refs already in CAS, or `None` for failures.
    ///
    /// On failures, one stamp per failed edge (synthesized via the
    /// row's [`BulkUpsertable::id`]) lands on the
    /// `<T::TABLE>_bookkeeping` sidecar inside the same flush tx.
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
        let mut error_stamps: Vec<(String, String)> = Vec::new();
        for (ref_id, err) in &self.errors {
            for (row_id, edge_ref_id) in &row_id_by_index {
                if edge_ref_id == ref_id {
                    error_stamps.push((row_id.clone(), err.clone()));
                }
            }
        }

        flush_cas_edges(pool, cas, &self.bundle.cas_inserts(), &rows, &error_stamps).await
    }
}

impl Default for CasEdgeAccumulator {
    fn default() -> Self {
        Self::new()
    }
}

// ─────────────────────────────────────────────────────────────────────
// CAS-edge flush primitive
// ─────────────────────────────────────────────────────────────────────

/// End-of-bucket CAS-edge flush. The shape every per-provider CAS
/// edge table (chatgpt_attachments, anthropic_attachments,
/// slack_attachments, chat_item_attachments) used to hand-roll
/// individually:
///
///   1. CAS pool: `put_many` so every edge row's `blake3` points at
///      bytes already in the CAS before the edge row lands.
///   2. Entity pool, single tx:
///      - `bulk_upsert_in_tx` the edge rows (writes + bookkeeping).
///      - For each `(id, err)` in `errors`, stamp `last_error` on
///        `<T::TABLE>_bookkeeping` via `record_object_attempt`. This
///        runs in the same tx so a failure here doesn't leave entity
///        rows without their error annotations.
///   3. Commit.
///
/// `T::TABLE` (from [`BulkUpsertable`]) is used as the bookkeeping
/// table name — every CAS-edge table has the standard
/// `<table>_bookkeeping` sidecar.
///
/// Caller's job is to pre-build the edge rows with the right blake3:
/// for fresh fetches the blake3 comes from `bundle.fetched_refs()`,
/// for refs whose bytes were already in CAS the caller looks it up
/// (e.g. via the provider's per-table `<ref_col>` query) and stamps
/// it forward — so every edge row carries the actual hash, not NULL.
/// Failures land in `errors` so the bookkeeping sidecar still
/// records what went wrong.
pub async fn flush_cas_edges<T: crate::bulk::BulkUpsertable>(
    pool: &SqlitePool,
    cas: &BlobCas,
    cas_inserts: &[CasInsert<'_>],
    rows: &[T],
    errors: &[(String, String)],
) -> Result<()> {
    if rows.is_empty() && cas_inserts.is_empty() && errors.is_empty() {
        return Ok(());
    }
    if !cas_inserts.is_empty() {
        cas.put_many(cas_inserts)
            .await
            .with_context(|| format!("flush_cas_edges put_many {}", T::TABLE))?;
    }
    let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
    let mut tx = pool
        .begin()
        .await
        .with_context(|| format!("begin flush_cas_edges {} tx", T::TABLE))?;
    crate::bulk::bulk_upsert_in_tx(&mut tx, rows, &now).await?;
    for (id, err) in errors {
        crate::doltlite_raw::record_object_attempt(&mut tx, T::TABLE, id, Some(err)).await?;
    }
    tx.commit()
        .await
        .with_context(|| format!("commit flush_cas_edges {} tx", T::TABLE))?;
    Ok(())
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
    fn cas_path_for_is_sibling_inside_dir() {
        let p = Path::new("/tmp/raw/slack/entities.doltlite_db");
        assert_eq!(
            cas_path_for(p),
            PathBuf::from("/tmp/raw/slack/blobs.doltlite_db")
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

    #[tokio::test(flavor = "multi_thread")]
    async fn bundle_load_round_trips_through_cas() {
        let d = tempdir().unwrap();
        let cas_path = d.path().join("cas.blobs.doltlite_db");
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

        let bundle = BlobBundle::load(
            &refs_pool,
            cas.pool(),
            "SELECT file_id AS ref_id, blake3, \
                    NULL AS content_type, upstream_name \
               FROM attachments \
              WHERE file_id IN ({placeholders})",
            &["a", "b", "missing"],
        )
        .await
        .unwrap();
        assert_eq!(bundle.len(), 2);
        let a = bundle.get("a").expect("a present");
        assert_eq!(a.bytes, b"alpha");
        // content_type comes from CAS when projection doesn't supply it
        assert_eq!(a.content_type.as_deref(), Some("image/png"));
        assert_eq!(a.upstream_name.as_deref(), Some("alpha.png"));
        assert!(bundle.get("missing").is_none());
    }
}
