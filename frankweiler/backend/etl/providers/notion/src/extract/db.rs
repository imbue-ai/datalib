//! Doltlite-backed raw store for the Notion provider.
//!
//! Replaces the per-entity JSONL trees with a single sqlx-managed
//! sqlite (eventually doltlite) file at `<data_root>/raw/<name>.doltlite_db`.
//! Schema is owned by this provider; the shared bookkeeping tables
//! (`blobs`, `sync_runs`) and the open / start_run
//! / blob plumbing live in [`frankweiler_etl::doltlite_raw`].
//!
//! See the module docs in `frankweiler_etl::doltlite_raw` for the
//! primary-key policy that governs every object table here.
//!
//! See `DOLTLITE_RAW.md` next to this crate for the design rationale.

use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use frankweiler_etl::blob_cas::{
    self, BlobCas, BlobReader, InMemoryBlobReader, RefStub, SqliteBlobReader,
};
use frankweiler_etl::doltlite_raw::{self as dr};

pub use frankweiler_etl::doltlite_raw::db_path_for;

use super::schema_raw::{full_ddl, DATA_TABLES};

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

    pub async fn blob_exists(&self, ref_id: &str) -> Result<bool> {
        blob_cas::ref_has_hash(&self.pool, ref_id).await
    }

    pub async fn store_blob(&self, stub: &RefStub<'_>, bytes: &[u8]) -> Result<String> {
        blob_cas::store_bytes(&self.pool, &self.cas, stub, bytes).await
    }

    /// Notion blobs are keyed `{block_id}:image`; we derive the
    /// owning_id and slot from that for the error sidecar so a retry
    /// path always has somewhere to look.
    pub async fn record_blob_error(&self, ref_id: &str, err: &str) -> Result<()> {
        let (owning, slot) = ref_id
            .rsplit_once(':')
            .map(|(o, s)| (o.to_string(), s.to_string()))
            .unwrap_or_else(|| (ref_id.to_string(), "image".to_string()));
        let mut tx = self.pool.begin().await.context("begin blob error tx")?;
        blob_cas::record_ref_error(&mut tx, ref_id, &owning, &slot, err).await?;
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
            let blobs: Arc<dyn BlobReader> = Arc::new(SqliteBlobReader::new(
                db.pool().clone(),
                db.cas().pool().clone(),
            ));
            Ok::<_, anyhow::Error>(LoadedRaw {
                pages,
                blocks,
                comments,
                blobs,
            })
        })
    })
}

/// Bag of payload arrays returned by [`block_on_load_all`]. Blob bytes
/// come through `blobs` as a streaming handle (one-at-a-time fetch),
/// not as a bulk HashMap.
#[derive(Clone)]
pub struct LoadedRaw {
    pub pages: Vec<Value>,
    pub blocks: Vec<(Value, Option<String>)>,
    pub comments: Vec<(Value, Option<String>)>,
    pub blobs: Arc<dyn BlobReader>,
}

impl Default for LoadedRaw {
    fn default() -> Self {
        Self {
            pages: Vec::new(),
            blocks: Vec::new(),
            comments: Vec::new(),
            blobs: InMemoryBlobReader::empty_handle(),
        }
    }
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
    fn db_path_for_handles_legacy_directory() {
        let p = std::path::Path::new("/tmp/raw/notion-api");
        assert_eq!(
            db_path_for(p),
            std::path::PathBuf::from("/tmp/raw/notion-api.doltlite_db")
        );
        let p2 = std::path::Path::new("/tmp/raw/notion-api.doltlite_db");
        assert_eq!(db_path_for(p2), p2);
    }
}
