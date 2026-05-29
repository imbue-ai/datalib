//! On-demand blob fetching for the translate step.
//!
//! Render needs blob bytes (PDFs, images, screenshots, etc.) only for
//! the documents it actually re-renders. A bulk `load_blobs_by_id` up
//! front means the bytes for every blob — possibly hundreds of MB —
//! sit in memory through the whole translate pass, even when most
//! documents are skipped by the fingerprint check. [`BlobStore`] is
//! the streaming alternative: one blob fetched at a time, on demand.
//!
//! Two implementations:
//!
//!   * [`SqliteBlobStore`] wraps the per-source raw doltlite pool and
//!     does a `SELECT … WHERE id = ?` per call. The sync interface
//!     uses `tokio::task::block_in_place` + `Handle::current().block_on`,
//!     so it can be called from translate code that's structured as
//!     synchronous Rust (matching the existing `translate_raw_dir` /
//!     `parse_api_dir` shape).
//!   * [`InMemoryBlobStore`] holds a `HashMap<id, BlobBytes>` and is
//!     used by provider unit tests that construct their own blob set.
//!
//! Render code calls `blobs.read_by_id(id)` or `blobs.read_by_owner(...)`
//! per blob it's about to materialize; peak RSS stays at one blob's
//! bytes instead of all of them.
//!
//! `read_by_owner` exists for providers (notion) that key blobs to the
//! parent block rather than to the blob's own UUID. When multiple
//! blobs share an `owning_id`, the SQL implementation picks the
//! lexically-last `id` — same convention the old bulk loader used.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;
use tokio::runtime::Handle;
use tokio::task;

use crate::doltlite_raw::BlobBytes;

/// Sync trait so render code (which runs under translate's synchronous
/// shape) can fetch blob bytes without threading async-ness through
/// every renderer. Implementations are responsible for any blocking
/// bridge they need (see [`SqliteBlobStore`]).
pub trait BlobStore: Send + Sync {
    fn read_by_id(&self, id: &str) -> Result<Option<BlobBytes>>;
    fn read_by_owner(&self, owning_id: &str) -> Result<Option<BlobBytes>>;
}

/// Per-blob sqlite reader. Holds a clone of the per-source doltlite
/// pool and runs one parametrized `SELECT` per call. Cheap to clone
/// (pool is internally `Arc`).
pub struct SqliteBlobStore {
    pool: SqlitePool,
}

impl SqliteBlobStore {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// Wrap in an `Arc<dyn BlobStore>` for hand-off into render code
    /// that stores blob handles as trait objects.
    pub fn into_handle(self) -> Arc<dyn BlobStore> {
        Arc::new(self)
    }

    /// Bridge an async sqlx call into our sync trait method. Caller
    /// must be inside a tokio runtime — translate code always is
    /// (slack-translate / chatgpt-translate / sync are all
    /// `#[tokio::main]`), so `Handle::current()` is reliable. The
    /// `block_in_place` keeps the worker thread free for other tasks
    /// that might be running on a multi-thread runtime.
    fn block_on<F: std::future::Future>(&self, fut: F) -> F::Output {
        task::block_in_place(|| Handle::current().block_on(fut))
    }
}

impl BlobStore for SqliteBlobStore {
    fn read_by_id(&self, id: &str) -> Result<Option<BlobBytes>> {
        self.block_on(async {
            let row = sqlx::query(
                "SELECT id, owning_id, slot, content_type, bytes, source_url \
                 FROM blobs WHERE id = ? AND bytes IS NOT NULL",
            )
            .bind(id)
            .fetch_optional(&self.pool)
            .await
            .with_context(|| format!("read_by_id {id}"))?;
            Ok(row.map(row_to_blob))
        })
    }

    fn read_by_owner(&self, owning_id: &str) -> Result<Option<BlobBytes>> {
        // Pick the lexically-last `id` for an owner, matching the
        // semantics the old bulk `load_blobs_by_owner` had — "only the
        // lexically-last id wins" for multi-blob owners.
        self.block_on(async {
            let row = sqlx::query(
                "SELECT id, owning_id, slot, content_type, bytes, source_url \
                 FROM blobs WHERE owning_id = ? AND bytes IS NOT NULL \
                 ORDER BY id DESC LIMIT 1",
            )
            .bind(owning_id)
            .fetch_optional(&self.pool)
            .await
            .with_context(|| format!("read_by_owner {owning_id}"))?;
            Ok(row.map(row_to_blob))
        })
    }
}

fn row_to_blob(r: sqlx::sqlite::SqliteRow) -> BlobBytes {
    BlobBytes {
        id: r.try_get("id").unwrap_or_default(),
        owning_id: r.try_get("owning_id").unwrap_or_default(),
        slot: r.try_get("slot").unwrap_or_default(),
        content_type: r.try_get("content_type").ok(),
        bytes: r.try_get("bytes").unwrap_or_default(),
        source_url: r.try_get("source_url").ok(),
    }
}

/// HashMap-backed store. Used in tests that build a literal blob set
/// in code rather than going through a doltlite. Cheap to construct
/// from a pre-existing `HashMap<String, BlobBytes>`.
pub struct InMemoryBlobStore {
    by_id: HashMap<String, BlobBytes>,
}

impl InMemoryBlobStore {
    pub fn new() -> Self {
        Self {
            by_id: HashMap::new(),
        }
    }

    /// Take ownership of a pre-built id→blob map.
    pub fn from_id_map(by_id: HashMap<String, BlobBytes>) -> Self {
        Self { by_id }
    }

    /// Build from a pre-built owner→blob map: collapses to the
    /// underlying by-id store, since each `BlobBytes` already carries
    /// its own id.
    pub fn from_owner_map(by_owner: HashMap<String, BlobBytes>) -> Self {
        let mut by_id = HashMap::with_capacity(by_owner.len());
        for (_owner, blob) in by_owner {
            by_id.insert(blob.id.clone(), blob);
        }
        Self { by_id }
    }

    pub fn insert(&mut self, blob: BlobBytes) {
        self.by_id.insert(blob.id.clone(), blob);
    }

    pub fn into_handle(self) -> Arc<dyn BlobStore> {
        Arc::new(self)
    }

    pub fn empty_handle() -> Arc<dyn BlobStore> {
        Arc::new(InMemoryBlobStore::new())
    }
}

impl Default for InMemoryBlobStore {
    fn default() -> Self {
        Self::new()
    }
}

impl BlobStore for InMemoryBlobStore {
    fn read_by_id(&self, id: &str) -> Result<Option<BlobBytes>> {
        Ok(self.by_id.get(id).cloned())
    }

    fn read_by_owner(&self, owning_id: &str) -> Result<Option<BlobBytes>> {
        // Match SqliteBlobStore's "lexically-last id wins" semantics.
        let pick = self
            .by_id
            .values()
            .filter(|b| b.owning_id == owning_id)
            .max_by(|a, b| a.id.cmp(&b.id))
            .cloned();
        Ok(pick)
    }
}
