//! Thin `RawDb` wrapper around the shared bulk/CAS/file-checkpoint
//! helpers.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::sqlite::SqlitePool;

use datalib_etl::blob_cas::{self, BlobCas};
use datalib_etl::doltlite_raw::{self as dr};

use super::schema_raw::{full_ddl, DATA_TABLES, EDGE_TABLES};

pub use datalib_etl::doltlite_raw::db_path_for;

/// Every cursor scope this provider owns. Reset wipes them in one go.
pub const CURSOR_SCOPE_PREFIX: &str = "google_takeout/";

#[derive(Clone, Debug)]
pub struct RawDb {
    pool: SqlitePool,
    cas: BlobCas,
}

impl RawDb {
    /// Open this store to *read* it, for the render pass.
    ///
    /// The download step owns this store; render only reads it. An ordinary
    /// [`Self::open`] would rescue-commit, reconcile the schema and commit
    /// again on the way in — three writes to a file this caller does not own,
    /// and once producers commit incrementally, a way to seal the
    /// downloader's half-written batch on its behalf. See
    /// `datalib_etl::doltlite_raw::open_reader`.
    ///
    /// No DDL, so a store the current downloader has not touched keeps
    /// whatever columns it has; probe with `column_exists` and fall back
    /// where that matters.
    pub async fn open_reader(db_path: &Path) -> Result<Self> {
        Ok(Self {
            pool: datalib_etl::doltlite_raw::open_reader(db_path).await?,
            cas: BlobCas::open_reader(&blob_cas::cas_path_for(db_path)).await?,
        })
    }

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

    /// Wait for both connections to actually go away, so the store can be
    /// reopened. Dropping the handle only schedules that.
    pub async fn close(self) {
        self.pool.close().await;
        self.cas.close().await;
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
        datalib_etl::file_checkpoint::clear_scope_prefix(&self.pool, CURSOR_SCOPE_PREFIX)
            .await
            .context("clear google_takeout file cursors on reset")?;
        Ok(())
    }

    // ── loads (consumed by render / tests) ───────────────────────

    pub async fn load_payloads(
        &self,
        reads: datalib_etl::pin::Reads<'_>,
        table: &str,
    ) -> Result<Vec<Value>> {
        dr::load_payloads(&self.pool, reads, table).await
    }

    /// Like [`Self::load_payloads`], but yields `(id, payload)` so the
    /// caller can join a row against a sibling table. Used by the chat
    /// renderer to map a `chat_groups` directory name to its members.
    pub async fn load_payloads_with_id(
        &self,
        reads: datalib_etl::pin::Reads<'_>,
        table: &str,
    ) -> Result<Vec<(String, Value)>> {
        dr::load_payloads_with_id(&self.pool, reads, table).await
    }
}
