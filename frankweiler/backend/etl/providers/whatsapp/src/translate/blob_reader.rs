//! Per-provider [`BlobReader`] for WhatsApp. Resolves an attachment's
//! sha256 (the `ref_id` translate stamps onto each `NormalizedAttachment`)
//! to a CAS blake3 via `wa_media_files`, then fetches bytes from
//! `cas_objects` on the sibling CAS pool. Replaces the shared
//! `SqliteBlobReader` (which went through the retired `blob_refs`
//! table).

use std::sync::Arc;

use anyhow::{Context, Result};
use frankweiler_etl::blob_cas::{BlobReader, BlobView};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

pub struct WhatsAppBlobReader {
    refs_pool: SqlitePool,
    cas_pool: SqlitePool,
}

impl WhatsAppBlobReader {
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

    async fn read_by_ref_id_async(&self, ref_id: &str) -> Result<Option<BlobView>> {
        let row = sqlx::query(
            "SELECT blake3, mime_type, relative_path \
               FROM wa_media_files \
              WHERE sha256 = ? AND blake3 IS NOT NULL \
              LIMIT 1",
        )
        .bind(ref_id)
        .fetch_optional(&self.refs_pool)
        .await
        .with_context(|| format!("lookup wa_media_files by sha256 {ref_id}"))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let blake3: String = row.try_get("blake3")?;
        let content_type: Option<String> = row.try_get("mime_type").ok().flatten();
        let upstream_name: Option<String> = row
            .try_get::<String, _>("relative_path")
            .ok()
            .and_then(|p| p.rsplit('/').next().map(str::to_string));

        let bytes_row = sqlx::query("SELECT bytes FROM cas_objects WHERE blake3 = ?")
            .bind(&blake3)
            .fetch_optional(&self.cas_pool)
            .await
            .with_context(|| format!("lookup cas_objects by blake3 {blake3}"))?;
        let Some(bytes_row) = bytes_row else {
            return Ok(None);
        };
        let bytes: Vec<u8> = bytes_row.try_get("bytes").unwrap_or_default();

        Ok(Some(BlobView {
            ref_id: ref_id.to_string(),
            owning_id: String::new(),
            slot: String::new(),
            blake3,
            content_type,
            upstream_name,
            source_url: None,
            bytes,
        }))
    }
}

impl BlobReader for WhatsAppBlobReader {
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
