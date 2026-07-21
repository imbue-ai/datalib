//! Doltlite-backed raw store for the Beeper provider.
//!
//! Single sqlite file at `<data_root>/<name>/raw/entities.doltlite_db`. Shared
//! bookkeeping tables (`blobs`, `sync_runs`) and
//! the open / blob plumbing live in [`frankweiler_etl::doltlite_raw`];
//! the primary-key policy is documented there.
//!
//! Beeper is multi-sourced: at least two SQLite stores on a typical
//! macOS install hold the data the desktop app shows you:
//!   * `~/Library/Application Support/BeeperTexts/index.db` — the
//!     desktop app's unified per-account message cache (covers cloud
//!     bridges like Slack/Google Chat AND local megabridges like
//!     Signal).
//!   * `~/Library/Messages/chat.db` — macOS's own iMessage SQLite,
//!     read directly by Beeper Texts via its platform-SDK integration.
//!     (Reader not yet implemented; needs Full Disk Access.)
//!
//! All readers feed into the same three object tables here. The
//! `source` column records which on-disk store a row originated from;
//! the `network` column is the canonical chat network (`"signal"`,
//! `"googlechat"`, `"slack"`, `"imessage"`, …) for downstream
//! filtering & dispatch.
//!
//! Tables:
//! - `rooms` — PK is `beeper_room_uuid(source, native_room_id)`. The
//!   native id (Matrix room id for index.db; chat.guid for Mac
//!   chat.db) lives alongside as its own column. Namespacing by
//!   `source` means two stores could in principle hold the "same"
//!   chat from different angles without colliding.
//! - `users` — PK is `beeper_user_uuid(source, native_user_id)`.
//! - `events` — PK is `beeper_event_uuid(source, native_event_id)`.
//!   `event_type` is the Beeper-canonical taxonomy (`TEXT`, `IMAGE`,
//!   `FILE`, `REACTION`, `MEMBERSHIP`, `HIDDEN`, …) — same labels the
//!   desktop app uses in `mx_room_messages.type`, so we don't have to
//!   reconstruct them from raw Matrix event shapes.

use std::path::Path;

use anyhow::{Context, Result};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use frankweiler_etl::blob_cas::{self, BlobCas};
use frankweiler_etl::bulk::bulk_upsert_in_tx;
use frankweiler_etl::doltlite_raw::{self as dr};

pub use frankweiler_etl::doltlite_raw::db_path_for;

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

    /// Distinct-row counts read straight from the destination DB
    /// after a run. Authoritative numbers for the summary —
    /// previous attempts at counting via `summary.events += 1`
    /// over-counted by 1 per reaction (since both
    /// `mx_room_messages` and `mx_reactions` upsert the same row
    /// independently and the second call doesn't decrement). Use
    /// these instead.
    pub async fn row_counts(&self) -> Result<RowCounts> {
        async fn one(pool: &SqlitePool, table: &str) -> Result<i64> {
            let row = sqlx::query(&format!("SELECT COUNT(*) AS n FROM {table}"))
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
        let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        let mut tx = self.pool.begin().await.context("begin bulk rooms tx")?;
        bulk_upsert_in_tx(&mut tx, rows, &now).await?;
        tx.commit().await.context("commit bulk rooms tx")?;
        Ok(())
    }

    /// Bulk-upsert users. See [`Self::bulk_upsert_rooms`] for the
    /// pattern.
    pub async fn bulk_upsert_users(&self, rows: &[UserRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        let mut tx = self.pool.begin().await.context("begin bulk users tx")?;
        bulk_upsert_in_tx(&mut tx, rows, &now).await?;
        tx.commit().await.context("commit bulk users tx")?;
        Ok(())
    }

    /// Bulk-upsert events. Hot path on a fresh ingest (thousands to
    /// tens of thousands of events per beeper account).
    pub async fn bulk_upsert_events(&self, rows: &[EventRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        let mut tx = self.pool.begin().await.context("begin bulk events tx")?;
        bulk_upsert_in_tx(&mut tx, rows, &now).await?;
        tx.commit().await.context("commit bulk events tx")?;
        Ok(())
    }

    /// Bulk-upsert attachment edge rows (`beeper_media_attachments`).
    /// Pair with [`BlobCas::put_many`] for the CAS bytes themselves.
    pub async fn bulk_upsert_media_attachments(
        &self,
        rows: &[BeeperMediaAttachmentRow],
    ) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
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
