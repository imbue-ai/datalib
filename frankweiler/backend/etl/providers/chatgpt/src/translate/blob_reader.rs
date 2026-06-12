//! ChatGPT-specific [`BlobReader`].
//!
//! **Lookup chain.** `read_by_ref_id(file_id)`:
//!
//! 1. `SELECT blake3 FROM chatgpt_attachments WHERE file_id = ?
//!    AND blake3 IS NOT NULL LIMIT 1` — resolve the upstream
//!    `file_id` to the CAS content hash (dedupe-aware across
//!    conversations that share the same `file_id`).
//! 2. `SELECT bytes, content_type FROM cas_objects WHERE blake3 = ?`
//!    — load the bytes and content_type the CAS stored at extract
//!    time.
//!
//! `read_by_owner` and `read_by_hash` return `Ok(None)` — the
//! renderer doesn't call them today.

use anyhow::{Context, Result};
use frankweiler_etl::blob_cas::{BlobReader, BlobView};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

pub struct ChatgptBlobReader {
    refs_pool: SqlitePool,
    cas_pool: SqlitePool,
}

impl ChatgptBlobReader {
    pub fn new(refs_pool: SqlitePool, cas_pool: SqlitePool) -> Self {
        Self {
            refs_pool,
            cas_pool,
        }
    }

    pub fn into_handle(self) -> std::sync::Arc<dyn BlobReader> {
        std::sync::Arc::new(self)
    }

    fn block_on<F: std::future::Future>(&self, fut: F) -> F::Output {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
    }

    async fn read_by_ref_id_async(&self, ref_id: &str) -> Result<Option<BlobView>> {
        let blake3: Option<String> = sqlx::query_scalar(
            "SELECT blake3 FROM chatgpt_attachments \
             WHERE file_id = ? AND blake3 IS NOT NULL LIMIT 1",
        )
        .bind(ref_id)
        .fetch_optional(&self.refs_pool)
        .await
        .with_context(|| format!("lookup chatgpt_attachments by file_id {ref_id}"))?;
        let Some(blake3) = blake3 else {
            return Ok(None);
        };
        let row = sqlx::query("SELECT bytes, content_type FROM cas_objects WHERE blake3 = ?")
            .bind(&blake3)
            .fetch_optional(&self.cas_pool)
            .await
            .with_context(|| format!("lookup cas_objects by blake3 {blake3}"))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let bytes: Vec<u8> = row.try_get("bytes").unwrap_or_default();
        let content_type: Option<String> = row.try_get("content_type").ok();
        Ok(Some(BlobView {
            ref_id: ref_id.to_string(),
            owning_id: String::new(),
            slot: String::new(),
            blake3,
            content_type,
            upstream_name: None,
            source_url: None,
            bytes,
        }))
    }
}

impl BlobReader for ChatgptBlobReader {
    fn read_by_ref_id(&self, ref_id: &str) -> Result<Option<BlobView>> {
        self.block_on(self.read_by_ref_id_async(ref_id))
    }
    fn read_by_owner(&self, _owning_id: &str) -> Result<Option<BlobView>> {
        Ok(None)
    }
    fn read_by_hash(&self, _blake3_hash: &str) -> Result<Option<BlobView>> {
        Ok(None)
    }
}
