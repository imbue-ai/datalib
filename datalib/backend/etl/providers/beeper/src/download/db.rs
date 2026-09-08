//! Doltlite-backed raw store for the Beeper provider.

use std::path::Path;

use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use datalib_etl::blob_cas::{self, BlobCas};
use datalib_etl::bulk::bulk_upsert_in_tx;
use datalib_etl::doltlite_raw::{self as dr};

pub use datalib_etl::doltlite_raw::db_path_for;

use super::schema_raw::{full_ddl, DATA_TABLES};
pub use super::schema_raw::{BeeperMediaAttachmentRow, EventRow, RoomRow, UserRow};

#[derive(Clone, Debug)]
pub struct RawDb {
    pool: SqlitePool,
    cas: BlobCas,
}

/// Distinct-row counts of every object table this provider
/// populates, taken from the destination DB after the run completes.
#[derive(Debug, Default, Clone, Copy)]
pub struct RowCounts {
    pub rooms: usize,
    pub users: usize,
    pub events: usize,
    pub blobs: usize,
    pub blob_errors: usize,
}

impl RawDb {
    pub async fn open(db_path: &Path) -> Result<Self> {
        let owned = full_ddl();
        let slices: Vec<&str> = owned.iter().map(String::as_str).collect();
        let pool = dr::open(db_path, &slices).await?;
        let cas = BlobCas::open(&blob_cas::cas_path_for(db_path)).await?;
        Ok(Self { pool, cas })
    }

    /// Wait for the connections to actually go away, so the store can be
    /// reopened. Dropping the handle only schedules that.
    pub async fn close(self) {
        self.pool.close().await;
        self.cas.close().await;
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn cas(&self) -> &BlobCas {
        &self.cas
    }

    pub async fn reset(&self) -> Result<()> {
        dr::truncate_data_tables(&self.pool, DATA_TABLES).await
    }

    /// Distinct-row counts read straight from the destination DB
    /// after a run. Authoritative numbers for the summary —
    /// previous attempts at counting via `summary.events += 1`
    /// over-counted by 1 per reaction (since both
    /// `mx_room_messages` and `mx_reactions` upsert the same row
    /// independently and the second call doesn't decrement). Use
    /// these instead.
    pub async fn row_counts(&self) -> Result<RowCounts> {
        async fn one(pool: &SqlitePool, table: &str) -> Result<i64> {
            // Audited: `table` is a literal at every callsite of this helper.
            let row = sqlx::query(sqlx::AssertSqlSafe(format!(
                "SELECT COUNT(*) AS n FROM {table}"
            )))
            .fetch_one(pool)
            .await
            .with_context(|| format!("count {table}"))?;
            Ok(row.try_get("n").unwrap_or(0))
        }
        let rooms = one(&self.pool, "rooms").await? as usize;
        let users = one(&self.pool, "users").await? as usize;
        let events = one(&self.pool, "events").await? as usize;
        let blobs = sqlx::query(
            "SELECT COUNT(*) AS n FROM beeper_media_attachments WHERE blake3 IS NOT NULL",
        )
        .fetch_one(&self.pool)
        .await
        .context("count beeper_media_attachments with bytes")?
        .try_get::<i64, _>("n")
        .unwrap_or(0) as usize;
        // No per-attachment error bookkeeping in the new edge table —
        // missing-media rows simply have a NULL blake3 (mirrors how
        // wa_media_files marks not-yet-fetched bytes). Failures bubble
        // up through the download `FetchSummary` directly.
        let blob_errors = 0;
        Ok(RowCounts {
            rooms,
            users,
            events,
            blobs,
            blob_errors,
        })
    }

    /// Bulk-upsert rooms in a single transaction via the shared
    /// [`bulk_upsert_in_tx`] helper. Rows must arrive with their
    /// UUIDv5 `id` already minted; see
    /// [`crate::render::beeper_room_uuid`].
    pub async fn bulk_upsert_rooms(&self, rows: &[RoomRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let now = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        let mut tx = self.pool.begin().await.context("begin bulk rooms tx")?;
        bulk_upsert_in_tx(&mut tx, rows, &now).await?;
        tx.commit().await.context("commit bulk rooms tx")?;
        Ok(())
    }

    pub async fn bulk_upsert_users(&self, rows: &[UserRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let now = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        let mut tx = self.pool.begin().await.context("begin bulk users tx")?;
        bulk_upsert_in_tx(&mut tx, rows, &now).await?;
        tx.commit().await.context("commit bulk users tx")?;
        Ok(())
    }

    pub async fn bulk_upsert_events(&self, rows: &[EventRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let now = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        let mut tx = self.pool.begin().await.context("begin bulk events tx")?;
        bulk_upsert_in_tx(&mut tx, rows, &now).await?;
        tx.commit().await.context("commit bulk events tx")?;
        Ok(())
    }

    pub async fn bulk_upsert_media_attachments(
        &self,
        rows: &[BeeperMediaAttachmentRow],
    ) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let now = datalib_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        let mut tx = self
            .pool
            .begin()
            .await
            .context("begin bulk media attachments tx")?;
        bulk_upsert_in_tx(&mut tx, rows, &now).await?;
        tx.commit()
            .await
            .context("commit bulk media attachments tx")?;
        Ok(())
    }
}
