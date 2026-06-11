//! Per-provider [`BlobReader`] for email. Mirrors signal's
//! `SignalBlobReader`: resolve a JMAP `blobId` to a CAS blake3 via
//! the new per-provider columns (`emails.blake3`,
//! `email_attachments.blake3`), then fetch the bytes from
//! `cas_objects` on the sibling CAS pool. Replaces the shared
//! `SqliteBlobReader` (which went through the now-retired
//! `blob_refs` table for this provider).

use anyhow::{Context, Result};
use frankweiler_etl::blob_cas::{BlobReader, BlobView};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

/// **Lookup chain.** `read_by_ref_id(blob_id)`:
///
/// 1. Query `emails.blake3` and `email_attachments.blake3` for the
///    given JMAP `blob_id`. The .eml source rides on `emails`, the
///    rest on `email_attachments`. The query unions both so a
///    single round-trip resolves either kind. If both have the same
///    `blob_id` we prefer `emails` (the body is the canonical
///    reference for a `blob_id` that means an .eml source).
/// 2. Look up the bytes in `cas_objects` keyed by `blake3`.
///
/// The returned [`BlobView`]'s `upstream_name` / `content_type`
/// come from the per-provider table (attachment `name` / `type` for
/// attachments, or NULL / `"message/rfc822"` for `.eml`). Render
/// uses them for `rendered_filename` and the markdown link
/// `display` text.
///
/// `read_by_owner` / `read_by_hash` return `Ok(None)` — render
/// doesn't call them today. Add when needed.
pub struct EmailBlobReader {
    refs_pool: SqlitePool,
    cas_pool: SqlitePool,
}

impl EmailBlobReader {
    pub fn new(refs_pool: SqlitePool, cas_pool: SqlitePool) -> Self {
        Self {
            refs_pool,
            cas_pool,
        }
    }

    fn block_on<F: std::future::Future>(&self, fut: F) -> F::Output {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
    }

    async fn read_by_ref_id_async(&self, ref_id: &str) -> Result<Option<BlobView>> {
        // Single round-trip: emails first (kind=eml, name=NULL, type
        // fixed), then email_attachments (kind=attachment, name/type
        // from the row). `LIMIT 1` picks at most one — emails first
        // because the UNION ALL preserves order.
        let row = sqlx::query(
            "SELECT blake3, NULL AS name, 'message/rfc822' AS content_type
               FROM emails
              WHERE blob_id = ? AND blake3 IS NOT NULL
              UNION ALL
              SELECT blake3, name, type AS content_type
                FROM email_attachments
               WHERE blob_id = ? AND blake3 IS NOT NULL
              LIMIT 1",
        )
        .bind(ref_id)
        .bind(ref_id)
        .fetch_optional(&self.refs_pool)
        .await
        .with_context(|| format!("lookup email blob by ref_id {ref_id}"))?;
        let Some(row) = row else {
            return Ok(None);
        };
        let blake3: String = row.try_get("blake3")?;
        let upstream_name: Option<String> = row.try_get("name").ok().flatten();
        let content_type: Option<String> = row.try_get("content_type").ok().flatten();

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

impl BlobReader for EmailBlobReader {
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
