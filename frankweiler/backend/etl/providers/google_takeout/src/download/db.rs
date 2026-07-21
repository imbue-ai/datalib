//! Thin `RawDb` wrapper around the shared bulk/CAS/file-checkpoint
//! helpers.
//!
//! Owns the entity-db pool + sibling CAS handle; all writes go
//! through [`frankweiler_etl::bulk`] /
//! [`frankweiler_etl::blob_cas`] from the per-feed walkers. The
//! provider has no SQL of its own.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::sqlite::SqlitePool;

use frankweiler_etl::blob_cas::{self, BlobCas};
use frankweiler_etl::doltlite_raw::{self as dr};

use super::schema_raw::{full_ddl, DATA_TABLES, EDGE_TABLES};

pub use frankweiler_etl::doltlite_raw::db_path_for;

/// Every cursor scope this provider owns. Reset wipes them in one go.
pub const CURSOR_SCOPE_PREFIX: &str = "google_takeout/";

#[derive(Clone, Debug)]
pub struct RawDb {
    pool: SqlitePool,
    cas: BlobCas,
}

impl RawDb {
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

    /// `--reset-and-redownload`. Truncates every entity / edge data
    /// table + bookkeeping sidecar and clears the per-feed file
    /// cursors. CAS bytes (`cas_objects`) survive — same convention
    /// as every other provider.
    pub async fn reset(&self) -> Result<()> {
        let all: Vec<&str> = DATA_TABLES
            .iter()
            .chain(EDGE_TABLES.iter())
            .copied()
            .collect();
        dr::truncate_data_tables(&self.pool, &all).await?;
        frankweiler_etl::file_checkpoint::clear_scope_prefix(&self.pool, CURSOR_SCOPE_PREFIX)
            .await
            .context("clear google_takeout file cursors on reset")?;
        Ok(())
    }

    // ── loads (consumed by render / tests) ───────────────────────

    pub async fn load_payloads(&self, table: &str) -> Result<Vec<Value>> {
        dr::load_payloads(&self.pool, table).await
    }

    /// Like [`Self::load_payloads`], but yields `(id, payload)` so the
    /// caller can join a row against a sibling table. Used by the chat
    /// renderer to map a `chat_groups` directory name to its members.
    pub async fn load_payloads_with_id(&self, table: &str) -> Result<Vec<(String, Value)>> {
        dr::load_payloads_with_id(&self.pool, table).await
    }
}
