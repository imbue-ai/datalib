//! Doltlite-backed raw store for the Notion provider.
//!
//! Replaces the per-entity JSONL trees with a single sqlx-managed
//! sqlite (eventually doltlite) file at `<data_root>/<name>/raw/entities.doltlite_db`.
//! Schema is owned by this provider; the shared bookkeeping tables
//! (`blobs`, `sync_runs`) and the open / start_run
//! / blob plumbing live in [`frankweiler_etl::doltlite_raw`].
//!
//! See the module docs in `frankweiler_etl::doltlite_raw` for the
//! primary-key policy that governs every object table here.
//!
//! See `DOLTLITE_RAW.md` next to this crate for the design rationale.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

use frankweiler_etl::blob_cas::{self, BlobBundle, BlobCas, CasEdgeRow as _};
use frankweiler_etl::doltlite_raw::{self as dr};

pub use frankweiler_etl::doltlite_raw::db_path_for;

use super::schema_raw::{full_ddl, NotionImageAttachmentRow, DATA_TABLES};

/// Handle on the raw-store sqlite file. Cheap to clone via the pool.
#[derive(Clone, Debug)]
pub struct RawDb {
    pool: SqlitePool,
    cas: BlobCas,
}

/// `(id, parent_id, last_edited_time, payload_json)` — one row for
/// [`RawDb::upsert_pages`]. `payload_json` is `None` for discovery
/// upserts that must not clobber a previously-fetched body.
pub type PageUpsertRow = (String, Option<String>, Option<String>, Option<String>);

/// What the extract loop wants to know about a page before it decides
/// whether to issue a detail fetch.
#[derive(Debug, Clone)]
pub struct PageState {
    pub last_edited_time: Option<String>,
    pub has_payload: bool,
}

/// One row of input to [`RawDb::upsert_blocks`]. `page_order` is the
/// 0-based index of this block within its owning page's BFS walk.
#[derive(Debug, Clone)]
pub struct BlockUpsert {
    pub id: String,
    pub parent_id: Option<String>,
    pub page_id: Option<String>,
    pub page_order: Option<i64>,
    pub last_edited_time: Option<String>,
    pub payload: Option<String>,
}

impl RawDb {
    /// Open (or create) the file at `db_path`, apply DDL idempotently.
    pub async fn open(db_path: &Path) -> Result<Self> {
        let owned = full_ddl();
        let slices: Vec<&str> = owned.iter().map(String::as_str).collect();
        let pool = dr::open(db_path, &slices).await?;
        let cas = BlobCas::open(&blob_cas::cas_path_for(db_path)).await?;
        Ok(Self { pool, cas })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn cas(&self) -> &BlobCas {
        &self.cas
    }

    /// Wipe every per-row table so the next fetch re-downloads
    /// everything from upstream. See
    /// [`frankweiler_etl::doltlite_raw::truncate_data_tables`].
    pub async fn reset(&self) -> Result<()> {
        dr::truncate_data_tables(&self.pool, DATA_TABLES).await
    }

    /// Snapshot every page's last_edited_time + payload-presence flag.
    /// Used at the start of a sync to decide which detail fetches we can
    /// skip.
    pub async fn page_states(&self) -> Result<std::collections::HashMap<String, PageState>> {
        let rows = sqlx::query(
            "SELECT id, last_edited_time, payload IS NOT NULL AS has_payload FROM pages",
        )
        .fetch_all(&self.pool)
        .await
        .context("select page_states")?;
        let mut out = std::collections::HashMap::with_capacity(rows.len());
        for r in rows {
            let id: String = r.try_get("id").unwrap_or_default();
            let last: Option<String> = r.try_get("last_edited_time").ok();
            let has: i64 = r.try_get("has_payload").unwrap_or(0);
            out.insert(
                id,
                PageState {
                    last_edited_time: last,
                    has_payload: has != 0,
                },
            );
        }
        Ok(out)
    }

    pub async fn ensure_id(&self, table: &str, id: &str) -> Result<()> {
        let mut tx = self.pool.begin().await.context("begin ensure_id tx")?;
        dr::ensure_object_row(&mut tx, table, id).await?;
        tx.commit().await.context("commit ensure_id tx")?;
        Ok(())
    }

    /// Batch upsert pages. We compare-on-upsert: if the stored
    /// `last_edited_time` already matches, we leave payload alone (the
    /// list pass shouldn't clobber a freshly-fetched detail body with a
    /// truncated list-only payload). When the incoming
    /// `last_edited_time` differs, payload is overwritten verbatim.
    pub async fn upsert_pages(&self, rows: &[PageUpsertRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.context("begin pages tx")?;
        for (id, parent_id, last_edited_time, payload) in rows {
            let sql = if payload.is_some() {
                "INSERT INTO pages (id, parent_id, last_edited_time, payload)
                 VALUES (?, ?, ?, jsonb(?))
                 ON CONFLICT(id) DO UPDATE SET
                    parent_id = COALESCE(excluded.parent_id, pages.parent_id),
                    last_edited_time = excluded.last_edited_time,
                    payload = excluded.payload"
            } else {
                "INSERT INTO pages (id, parent_id, last_edited_time)
                 VALUES (?, ?, ?)
                 ON CONFLICT(id) DO UPDATE SET
                    parent_id = COALESCE(excluded.parent_id, pages.parent_id),
                    last_edited_time = COALESCE(excluded.last_edited_time, pages.last_edited_time)"
            };
            let mut q = sqlx::query(sql)
                .bind(id)
                .bind(parent_id)
                .bind(last_edited_time);
            if let Some(p) = payload {
                q = q.bind(p);
            }
            q.execute(&mut *tx)
                .await
                .with_context(|| format!("upsert page {id}"))?;
            // Sidecar update: success attempt when payload arrived,
            // bare pre-seed (attempt_count=0) otherwise.
            if payload.is_some() {
                dr::record_object_attempt(&mut tx, "pages", id, None).await?;
            } else {
                sqlx::query("INSERT OR IGNORE INTO pages_bookkeeping (id) VALUES (?)")
                    .bind(id)
                    .execute(&mut *tx)
                    .await
                    .with_context(|| format!("pre-seed pages_bookkeeping {id}"))?;
            }
        }
        tx.commit().await.context("commit pages tx")?;
        Ok(())
    }

    /// Batch upsert blocks. Detail-only — list+detail in one shot since
    /// Notion's `/blocks/{id}/children` returns full block bodies.
    pub async fn upsert_blocks(&self, rows: &[BlockUpsert]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.context("begin blocks tx")?;
        for BlockUpsert {
            id,
            parent_id,
            page_id,
            page_order,
            last_edited_time,
            payload,
        } in rows
        {
            sqlx::query(
                "INSERT INTO blocks (id, parent_id, page_id, page_order, last_edited_time, payload)
                 VALUES (?, ?, ?, ?, ?, jsonb(?))
                 ON CONFLICT(id) DO UPDATE SET
                    parent_id = COALESCE(excluded.parent_id, blocks.parent_id),
                    page_id = COALESCE(excluded.page_id, blocks.page_id),
                    page_order = COALESCE(excluded.page_order, blocks.page_order),
                    last_edited_time = excluded.last_edited_time,
                    payload = excluded.payload",
            )
            .bind(id)
            .bind(parent_id)
            .bind(page_id)
            .bind(page_order)
            .bind(last_edited_time)
            .bind(payload)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("upsert block {id}"))?;
            dr::record_object_attempt(&mut tx, "blocks", id, None).await?;
        }
        tx.commit().await.context("commit blocks tx")?;
        Ok(())
    }

    /// Batch upsert comments — also detail-in-list.
    pub async fn upsert_comments(
        &self,
        rows: &[(String, String, Option<String>, String)],
    ) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.context("begin comments tx")?;
        for (id, parent_id, page_id, payload) in rows {
            sqlx::query(
                "INSERT INTO comments (id, parent_id, page_id, payload)
                 VALUES (?, ?, ?, jsonb(?))
                 ON CONFLICT(id) DO UPDATE SET
                    parent_id = excluded.parent_id,
                    page_id = COALESCE(excluded.page_id, comments.page_id),
                    payload = excluded.payload",
            )
            .bind(id)
            .bind(parent_id)
            .bind(page_id)
            .bind(payload)
            .execute(&mut *tx)
            .await
            .with_context(|| format!("upsert comment {id}"))?;
            dr::record_object_attempt(&mut tx, "comments", id, None).await?;
        }
        tx.commit().await.context("commit comments tx")?;
        Ok(())
    }

    pub async fn record_page_error(&self, id: &str, err: &str) -> Result<()> {
        let mut tx = self
            .pool
            .begin()
            .await
            .context("begin record_page_error tx")?;
        dr::record_object_error(&mut tx, "pages", id, err).await?;
        tx.commit().await.context("commit record_page_error tx")?;
        Ok(())
    }

    pub async fn failed_page_ids(&self) -> Result<Vec<String>> {
        dr::failed_ids(&self.pool, "pages").await
    }

    pub async fn load_pages(&self) -> Result<Vec<Value>> {
        dr::load_payloads(&self.pool, "pages").await
    }

    /// Stored child-page block ids for `page_id`. Used by the
    /// "unchanged page" skip path: when a page's `last_edited_time`
    /// hasn't moved since our last fetch, we elide the block walk —
    /// but the BFS still needs to recurse into known children in case
    /// a *child*'s `last_edited_time` advanced even when the parent's
    /// did not.
    pub async fn stored_child_page_ids(&self, page_id: &str) -> Result<Vec<String>> {
        let rows = sqlx::query(
            "SELECT json_extract(payload, '$.id') AS id \
             FROM blocks \
             WHERE page_id = ? \
               AND json_extract(payload, '$.type') = 'child_page' \
               AND payload IS NOT NULL",
        )
        .bind(page_id)
        .fetch_all(&self.pool)
        .await
        .context("select stored child_page block ids")?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            if let Ok(id) = r.try_get::<String, _>("id") {
                out.push(id);
            }
        }
        Ok(out)
    }

    pub async fn load_blocks(&self) -> Result<Vec<(Value, Option<String>)>> {
        // ORDER BY (page_id, page_order) reproduces BFS discovery
        // order from extract/mod.rs::walk_page_blocks; render relies
        // on this for section / toggle layout. `id` ties the tail so
        // results stay deterministic when page_order is NULL.
        let rows = sqlx::query(
            "SELECT json(payload) AS payload, page_id FROM blocks WHERE payload IS NOT NULL \
             ORDER BY page_id, page_order, id",
        )
        .fetch_all(&self.pool)
        .await
        .context("select blocks")?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let payload: String = match r.try_get("payload") {
                Ok(s) => s,
                Err(_) => continue,
            };
            let page_id: Option<String> = r.try_get("page_id").ok();
            if let Ok(v) = serde_json::from_str::<Value>(&payload) {
                out.push((v, page_id));
            }
        }
        Ok(out)
    }

    pub async fn load_comments(&self) -> Result<Vec<(Value, Option<String>)>> {
        let rows = sqlx::query(
            "SELECT json(payload) AS payload, page_id FROM comments WHERE payload IS NOT NULL ORDER BY id",
        )
        .fetch_all(&self.pool)
        .await
        .context("select comments")?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let payload: String = match r.try_get("payload") {
                Ok(s) => s,
                Err(_) => continue,
            };
            let page_id: Option<String> = r.try_get("page_id").ok();
            if let Ok(v) = serde_json::from_str::<Value>(&payload) {
                out.push((v, page_id));
            }
        }
        Ok(out)
    }

    /// Have we already stored bytes for this image-block's ref_id?
    /// One SELECT against `notion_image_attachments` — the universal
    /// CAS-edge "have we got these bytes yet?" skip-check shape every
    /// other ported provider uses (`wa_media_files`, `slack_attachments`,
    /// …). NULL `blake3` means "we know the ref exists but haven't
    /// fetched bytes yet" — returns false so the caller fetches.
    pub async fn blob_exists(&self, ref_id: &str) -> Result<bool> {
        let row = sqlx::query(
            "SELECT 1 FROM notion_image_attachments \
             WHERE ref_id = ? AND blake3 IS NOT NULL LIMIT 1",
        )
        .bind(ref_id)
        .fetch_optional(&self.pool)
        .await
        .context("notion_image_attachments skip-check")?;
        Ok(row.is_some())
    }

    /// Hash + store the bytes in the per-source CAS, then land an edge
    /// row on `notion_image_attachments`. No writes to the shared
    /// `blob_refs` table — Notion uses the per-provider edge shape
    /// every other provider settled on.
    pub async fn store_blob(
        &self,
        block_id: &str,
        ref_id: &str,
        content_type: Option<&str>,
        bytes: &[u8],
    ) -> Result<String> {
        let hash = self.cas.put(bytes, content_type).await?;
        let edge = NotionImageAttachmentRow {
            id: NotionImageAttachmentRow::pk_recipe(block_id, ref_id),
            block_id: block_id.to_string(),
            ref_id: ref_id.to_string(),
            blake3: Some(hash.clone()),
        };
        let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        let mut tx = self
            .pool
            .begin()
            .await
            .context("begin notion_image_attachments tx")?;
        frankweiler_etl::bulk::bulk_upsert_in_tx(&mut tx, &[edge], &now).await?;
        tx.commit()
            .await
            .context("commit notion_image_attachments tx")?;
        Ok(hash)
    }

    /// Record a known-but-not-yet-fetched edge row so a future retry
    /// has something to look at. Mirrors how WhatsApp / Beeper handle
    /// "we know about this attachment but haven't pulled bytes" —
    /// blake3 stays NULL until the CAS write lands.
    pub async fn record_blob_error(&self, block_id: &str, ref_id: &str) -> Result<()> {
        let edge = NotionImageAttachmentRow {
            id: NotionImageAttachmentRow::pk_recipe(block_id, ref_id),
            block_id: block_id.to_string(),
            ref_id: ref_id.to_string(),
            blake3: None,
        };
        let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        let mut tx = self.pool.begin().await.context("begin blob error tx")?;
        frankweiler_etl::bulk::bulk_upsert_in_tx(&mut tx, &[edge], &now).await?;
        tx.commit().await.context("commit blob error tx")?;
        Ok(())
    }
}

/// Synchronous helper for non-async callers (translate, synthesize) that
/// already run under `#[tokio::main]`. Uses `block_in_place` + the
/// current Handle, so it must be invoked on a multi-thread runtime.
pub fn block_on_load_all(db_path: &Path) -> Result<LoadedRaw> {
    let path = db_path.to_path_buf();
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let db = RawDb::open(&path).await?;
            let pages = db.load_pages().await?;
            let blocks = db.load_blocks().await?;
            let comments = db.load_comments().await?;
            let blobs_by_page =
                load_blobs_by_page(db.pool(), &blob_cas::cas_path_for(&path), &blocks).await?;
            Ok::<_, anyhow::Error>(LoadedRaw {
                pages,
                blocks,
                comments,
                blobs_by_page,
            })
        })
    })
}

/// SQL projection used by [`BlobBundle::load`] to map an image
/// block's `ref_id` (`"{block_uuid}:image"`) to its CAS `blake3`.
const ATTACHMENTS_PROJECTION_SQL: &str = "
    SELECT ref_id, blake3,
           NULL AS content_type,
           NULL AS upstream_name
      FROM notion_image_attachments
     WHERE ref_id IN ({placeholders}) AND blake3 IS NOT NULL";

/// Build the per-page BlobBundle map render reads from. Walks every
/// loaded block's `(page_id, block_id)` pair, derives the
/// `"{block_id}:image"` ref_id convention extract uses, and per-page
/// loads a BlobBundle from the sibling CAS via
/// `ATTACHMENTS_PROJECTION_SQL`. Pages with no image blocks get no
/// entry (render falls through to the upstream-URL placeholder).
async fn load_blobs_by_page(
    refs_pool: &SqlitePool,
    cas_path: &Path,
    blocks: &[(Value, Option<String>)],
) -> Result<HashMap<String, BlobBundle>> {
    let mut by_page: HashMap<String, Vec<String>> = HashMap::new();
    for (block, page_id) in blocks {
        let Some(page_id) = page_id.as_deref() else {
            continue;
        };
        if block.get("type").and_then(|v| v.as_str()) != Some("image") {
            continue;
        }
        let Some(block_id) = block.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        by_page
            .entry(page_id.to_string())
            .or_default()
            .push(format!("{block_id}:image"));
    }
    if by_page.is_empty() || !cas_path.is_file() {
        return Ok(HashMap::new());
    }
    let cas_opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", cas_path.display()))
        .with_context(|| format!("sqlite uri for {}", cas_path.display()))?
        .read_only(true);
    let cas_pool: SqlitePool = SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(60))
        .connect_with(cas_opts)
        .await
        .with_context(|| format!("open CAS for translate at {}", cas_path.display()))?;
    let mut out: HashMap<String, BlobBundle> = HashMap::new();
    for (page_id, refs) in by_page {
        let mut seen: HashSet<&str> = HashSet::new();
        let refs_vec: Vec<&str> = refs
            .iter()
            .map(String::as_str)
            .filter(|r| seen.insert(*r))
            .collect();
        let bundle =
            BlobBundle::load(refs_pool, &cas_pool, ATTACHMENTS_PROJECTION_SQL, &refs_vec).await?;
        if !bundle.is_empty() {
            out.insert(page_id, bundle);
        }
    }
    cas_pool.close().await;
    Ok(out)
}

/// Bag of payload arrays returned by [`block_on_load_all`]. Attachment
/// bytes arrive per-page in `blobs_by_page` — one `BlobBundle` per
/// page that has at least one image block whose bytes are in the
/// CAS — same shape slack / whatsapp / email use.
#[derive(Clone, Default)]
pub struct LoadedRaw {
    pub pages: Vec<Value>,
    pub blocks: Vec<(Value, Option<String>)>,
    pub comments: Vec<(Value, Option<String>)>,
    pub blobs_by_page: HashMap<String, BlobBundle>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn open_creates_file_and_tables() {
        let dir = tempfile::tempdir().unwrap();
        let db_file = dir.path().join("notion-api.doltlite_db");
        let db = RawDb::open(&db_file).await.unwrap();
        assert!(db_file.exists());
        let pages = db.load_pages().await.unwrap();
        assert!(pages.is_empty());
    }

    #[tokio::test]
    async fn upsert_page_then_load_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let db = RawDb::open(&dir.path().join("x.doltlite_db"))
            .await
            .unwrap();
        db.upsert_pages(&[(
            "p1".into(),
            Some("root".into()),
            Some("2026-05-21T19:37:00Z".into()),
            Some(serde_json::to_string(&json!({"id": "p1", "title": "hi"})).unwrap()),
        )])
        .await
        .unwrap();
        let states = db.page_states().await.unwrap();
        assert!(states.get("p1").unwrap().has_payload);
        let pages = db.load_pages().await.unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0]["title"], "hi");
    }

    #[tokio::test]
    async fn store_blob_succeeds_and_records_edge() {
        // Regression: `notion_image_attachments` was created without its
        // paired `_bookkeeping` sidecar (the table was missing from
        // DATA_TABLES, unlike every other provider's CAS-edge table). So
        // `store_blob` -> `bulk_upsert_in_tx` -> `bulk_upsert_bookkeeping`
        // failed with "no such table: notion_image_attachments_bookkeeping"
        // and the fetched image bytes were dropped on every run.
        let dir = tempfile::tempdir().unwrap();
        let db = RawDb::open(&dir.path().join("blob.doltlite_db"))
            .await
            .unwrap();
        let block_id = "364a550f-af95-8007-9bac-f40d5d9eb53c";
        let ref_id = format!("{block_id}:image");
        assert!(
            !db.blob_exists(&ref_id).await.unwrap(),
            "edge should not exist before store"
        );
        let hash = db
            .store_blob(block_id, &ref_id, Some("image/png"), b"\x89PNG fake bytes")
            .await
            .expect("store_blob should succeed once the bookkeeping sidecar exists");
        assert_eq!(hash.len(), 64, "blake3 hex hash");
        assert!(
            db.blob_exists(&ref_id).await.unwrap(),
            "edge with non-null blake3 should exist after store"
        );
    }

    #[tokio::test]
    async fn record_page_error_bumps_attempt_count() {
        let dir = tempfile::tempdir().unwrap();
        let db = RawDb::open(&dir.path().join("y.doltlite_db"))
            .await
            .unwrap();
        db.record_page_error("p1", "boom").await.unwrap();
        db.record_page_error("p1", "boom2").await.unwrap();
        let failed = db.failed_page_ids().await.unwrap();
        assert_eq!(failed, vec!["p1".to_string()]);
    }

    #[tokio::test]
    async fn successful_upsert_clears_last_error() {
        let dir = tempfile::tempdir().unwrap();
        let db = RawDb::open(&dir.path().join("z.doltlite_db"))
            .await
            .unwrap();
        db.record_page_error("p1", "fail").await.unwrap();
        db.upsert_pages(&[(
            "p1".into(),
            None,
            Some("2026-01-01T00:00:00Z".into()),
            Some("{}".into()),
        )])
        .await
        .unwrap();
        let failed = db.failed_page_ids().await.unwrap();
        assert!(failed.is_empty());
    }

    #[tokio::test]
    async fn payload_is_stored_as_jsonb_blob() {
        // After upserting via `jsonb(?)`, the stored payload column
        // should be a BLOB (jsonb's binary representation), not TEXT.
        // Guards against silently falling back to plain JSON text when
        // someone unwraps the `jsonb()` call from an INSERT.
        let dir = tempfile::tempdir().unwrap();
        let db = RawDb::open(&dir.path().join("j.doltlite_db"))
            .await
            .unwrap();
        db.upsert_pages(&[(
            "p1".into(),
            None,
            Some("2026-01-01T00:00:00Z".into()),
            Some(serde_json::to_string(&json!({"a": [1, 2, 3], "b": "hi"})).unwrap()),
        )])
        .await
        .unwrap();
        let row = sqlx::query("SELECT typeof(payload) AS t FROM pages WHERE id='p1'")
            .fetch_one(db.pool())
            .await
            .unwrap();
        let t: String = row.try_get("t").unwrap();
        assert_eq!(t, "blob", "payload should be JSONB-encoded BLOB");
    }

    #[test]
    fn db_path_for_places_db_inside_directory() {
        let p = std::path::Path::new("/tmp/raw/notion-api");
        assert_eq!(
            db_path_for(p),
            std::path::PathBuf::from("/tmp/raw/notion-api/entities.doltlite_db")
        );
        let p2 = std::path::Path::new("/tmp/raw/notion-api/entities.doltlite_db");
        assert_eq!(db_path_for(p2), p2);
    }
}
