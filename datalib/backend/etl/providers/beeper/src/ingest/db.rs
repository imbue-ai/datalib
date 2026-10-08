//! Doltlite-backed raw store for the Beeper provider. Every run reads the
//! whole cache, so a row's sidecar is stamped the first time it is seen
//! and reading an unchanged cache again commits nothing.

use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use datalib_etl::blob_cas::{CasEdgeAccumulator, CasEdgeRow as _};
use datalib_etl::bulk::bulk_upsert_first_seen_in_tx;

pub use datalib_etl::doltlite_raw::db_path_for;

use super::schema_raw::full_ddl;
pub use super::schema_raw::{BeeperMediaAttachmentRow, EventRow, RoomRow, UserRow};

datalib_etl::raw_db!(pub RawDb: CasEntityStore, full_ddl());

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
        let rooms = one(self.pool(), "rooms").await? as usize;
        let users = one(self.pool(), "users").await? as usize;
        let events = one(self.pool(), "events").await? as usize;
        let blobs = sqlx::query(
            "SELECT COUNT(*) AS n FROM beeper_media_attachments WHERE blake3 IS NOT NULL",
        )
        .fetch_one(self.pool())
        .await
        .context("count beeper_media_attachments with bytes")?
        .try_get::<i64, _>("n")
        .unwrap_or(0) as usize;
        let blob_errors = sqlx::query(
            "SELECT COUNT(*) AS n FROM beeper_media_attachments_bookkeeping \
             WHERE last_error IS NOT NULL",
        )
        .fetch_one(self.pool())
        .await
        .context("count beeper_media_attachments that did not copy")?
        .try_get::<i64, _>("n")
        .unwrap_or(0) as usize;
        Ok(RowCounts {
            rooms,
            users,
            events,
            blobs,
            blob_errors,
        })
    }

    /// A row's `id` is its native room id.
    pub async fn bulk_upsert_rooms(&self, rows: &[RoomRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let now = datalib_time::IsoOffsetTimestamp::now_local();
        let mut tx = self.pool().begin().await.context("begin bulk rooms tx")?;
        bulk_upsert_first_seen_in_tx(&mut tx, rows, &now).await?;
        tx.commit().await.context("commit bulk rooms tx")?;
        Ok(())
    }

    pub async fn bulk_upsert_users(&self, rows: &[UserRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let now = datalib_time::IsoOffsetTimestamp::now_local();
        let mut tx = self.pool().begin().await.context("begin bulk users tx")?;
        bulk_upsert_first_seen_in_tx(&mut tx, rows, &now).await?;
        tx.commit().await.context("commit bulk users tx")?;
        Ok(())
    }

    pub async fn bulk_upsert_events(&self, rows: &[EventRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let now = datalib_time::IsoOffsetTimestamp::now_local();
        let mut tx = self.pool().begin().await.context("begin bulk events tx")?;
        bulk_upsert_first_seen_in_tx(&mut tx, rows, &now).await?;
        tx.commit().await.context("commit bulk events tx")?;
        Ok(())
    }

    /// Land one thread's attachment edges and the bytes that were read;
    /// one that was not is an edge with no bytes and a `problems` row
    /// until a later run reads it.
    pub async fn flush_media_attachments(&self, acc: &CasEdgeAccumulator) -> Result<()> {
        acc.flush(self.pool(), self.cas(), |event_uuid, ref_id, blake3| {
            BeeperMediaAttachmentRow {
                id: BeeperMediaAttachmentRow::pk_recipe(event_uuid, ref_id),
                event_uuid: event_uuid.to_string(),
                ref_id: ref_id.to_string(),
                blake3: blake3.map(String::from),
            }
        })
        .await
    }

    pub async fn bulk_upsert_media_attachments(
        &self,
        rows: &[BeeperMediaAttachmentRow],
    ) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let now = datalib_time::IsoOffsetTimestamp::now_local();
        let mut tx = self
            .pool()
            .begin()
            .await
            .context("begin bulk media attachments tx")?;
        bulk_upsert_first_seen_in_tx(&mut tx, rows, &now).await?;
        tx.commit()
            .await
            .context("commit bulk media attachments tx")?;
        Ok(())
    }
}
