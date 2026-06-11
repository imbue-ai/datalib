//! Doltlite-backed raw store for the Beeper provider.
//!
//! Single sqlite file at `<data_root>/raw/<name>.doltlite_db`. Shared
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
use serde_json::Value;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use frankweiler_etl::blob_cas::{self, BlobCas};
use frankweiler_etl::bulk::{bulk_upsert_bookkeeping, SQL_CHUNK};
use frankweiler_etl::doltlite_raw::{self as dr};

pub use frankweiler_etl::doltlite_raw::db_path_for;

use super::schema_raw::{full_ddl, DATA_TABLES};
use crate::translate::{beeper_event_uuid, beeper_room_uuid, beeper_user_uuid};

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

/// Distilled room row going into the `rooms` table. Caller fills in
/// every column it knows; defaults are fine for the rest.
#[derive(Debug, Clone, Default)]
pub struct RoomRow {
    pub source: String,
    pub network: String,
    pub native_room_id: String,
    /// Upstream system's canonical id for this room (Signal
    /// conversation UUID, Slack channel id, Google Chat space id,
    /// etc.). See the DDL comment for the distinction from
    /// `native_room_id`.
    pub external_room_id: Option<String>,
    /// Upstream workspace/account id (Signal account UUID, Slack
    /// team id, …). For bridges with a flat per-account namespace
    /// (e.g. Google Chat as we currently see it) this is `None`.
    pub external_workspace_id: Option<String>,
    pub account_id: Option<String>,
    pub room_type: Option<String>,
    pub title: Option<String>,
    pub description: Option<String>,
    pub is_dm: bool,
    pub is_space: bool,
    /// Full upstream record (the `thread` JSON for index.db, etc.).
    pub payload: Value,
}

#[derive(Debug, Clone, Default)]
pub struct UserRow {
    pub source: String,
    pub network: Option<String>,
    pub native_user_id: String,
    pub display_name: Option<String>,
    pub full_name: Option<String>,
    pub remote_id: Option<String>,
    pub avatar_blob_id: Option<String>,
    pub payload: Value,
}

#[derive(Debug, Clone, Default)]
pub struct EventRow {
    pub source: String,
    pub network: String,
    pub native_room_id: String,
    pub native_event_id: String,
    /// Upstream system's canonical message id. None for
    /// `beeper_index` rows — Beeper Texts doesn't propagate the
    /// underlying network's per-message ids into its desktop cache.
    /// Reserved here so future bridges-DB or cloud-API readers can
    /// backfill without a schema bump.
    pub external_event_id: Option<String>,
    pub sender_native_user_id: Option<String>,
    pub event_type: String,
    pub timestamp_ms: i64,
    pub text_content: Option<String>,
    pub reply_to_native_event_id: Option<String>,
    pub edit_of_native_event_id: Option<String>,
    pub reaction_emoji: Option<String>,
    pub reaction_target_native_event_id: Option<String>,
    pub payload: Value,
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
        let blobs = sqlx::query("SELECT COUNT(*) AS n FROM blob_refs WHERE blake3 IS NOT NULL")
            .fetch_one(&self.pool)
            .await
            .context("count blobs with bytes")?
            .try_get::<i64, _>("n")
            .unwrap_or(0) as usize;
        let blob_errors = sqlx::query(
            "SELECT COUNT(*) AS n FROM blob_refs_bookkeeping WHERE last_error IS NOT NULL",
        )
        .fetch_one(&self.pool)
        .await
        .context("count blobs with errors")?
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

    /// Bulk-upsert rooms via chunked multi-row INSERT inside one tx.
    /// The synthesized PK is computed eagerly per row from
    /// `(source, native_room_id)` via [`beeper_room_uuid`].
    pub async fn bulk_upsert_rooms(&self, rows: &[RoomRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        // Precompute (id, payload_text) per row so the bind loop
        // below can borrow stable slices.
        let prepared: Vec<(String, String)> = rows
            .iter()
            .map(|r| {
                let id = beeper_room_uuid(&r.source, &r.native_room_id);
                let payload =
                    serde_json::to_string(&r.payload).unwrap_or_else(|_| "null".to_string());
                (id, payload)
            })
            .collect();
        let mut tx = self.pool.begin().await.context("begin bulk rooms tx")?;
        for (chunk, prepared_chunk) in rows.chunks(SQL_CHUNK).zip(prepared.chunks(SQL_CHUNK)) {
            let mut sql = String::from(
                "INSERT INTO rooms (
                    id, source, network, native_room_id,
                    external_room_id, external_workspace_id,
                    account_id, room_type, title, description, is_dm, is_space,
                    payload
                ) VALUES ",
            );
            // 12 plain placeholders + jsonb(?) for payload.
            for i in 0..chunk.len() {
                if i > 0 {
                    sql.push(',');
                }
                sql.push_str("(?,?,?,?,?,?,?,?,?,?,?,?,jsonb(?))");
            }
            sql.push_str(
                " ON CONFLICT(id) DO UPDATE SET
                    network               = excluded.network,
                    external_room_id      = COALESCE(excluded.external_room_id, rooms.external_room_id),
                    external_workspace_id = COALESCE(excluded.external_workspace_id, rooms.external_workspace_id),
                    account_id            = COALESCE(excluded.account_id, rooms.account_id),
                    room_type             = COALESCE(excluded.room_type, rooms.room_type),
                    title                 = excluded.title,
                    description           = excluded.description,
                    is_dm                 = excluded.is_dm,
                    is_space              = excluded.is_space,
                    payload               = excluded.payload",
            );
            let mut q = sqlx::query(&sql);
            for (r, (id, payload_txt)) in chunk.iter().zip(prepared_chunk.iter()) {
                q = q
                    .bind(id)
                    .bind(&r.source)
                    .bind(&r.network)
                    .bind(&r.native_room_id)
                    .bind(r.external_room_id.as_deref())
                    .bind(r.external_workspace_id.as_deref())
                    .bind(r.account_id.as_deref())
                    .bind(r.room_type.as_deref())
                    .bind(r.title.as_deref())
                    .bind(r.description.as_deref())
                    .bind(r.is_dm as i64)
                    .bind(r.is_space as i64)
                    .bind(payload_txt);
            }
            q.execute(&mut *tx).await.context("bulk insert rooms")?;
        }
        bulk_upsert_bookkeeping(
            &mut tx,
            "rooms",
            prepared.iter().map(|(id, _)| id.as_str()),
            &now,
        )
        .await?;
        tx.commit().await.context("commit bulk rooms tx")?;
        Ok(())
    }

    /// Bulk-upsert users via chunked multi-row INSERT inside one tx.
    pub async fn bulk_upsert_users(&self, rows: &[UserRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        let prepared: Vec<(String, String)> = rows
            .iter()
            .map(|r| {
                let id = beeper_user_uuid(&r.source, &r.native_user_id);
                let payload =
                    serde_json::to_string(&r.payload).unwrap_or_else(|_| "null".to_string());
                (id, payload)
            })
            .collect();
        let mut tx = self.pool.begin().await.context("begin bulk users tx")?;
        for (chunk, prepared_chunk) in rows.chunks(SQL_CHUNK).zip(prepared.chunks(SQL_CHUNK)) {
            let mut sql = String::from(
                "INSERT INTO users (
                    id, source, network, native_user_id,
                    display_name, full_name, remote_id, avatar_blob_id,
                    payload
                ) VALUES ",
            );
            for i in 0..chunk.len() {
                if i > 0 {
                    sql.push(',');
                }
                sql.push_str("(?,?,?,?,?,?,?,?,jsonb(?))");
            }
            sql.push_str(
                " ON CONFLICT(id) DO UPDATE SET
                    network         = COALESCE(excluded.network, users.network),
                    display_name    = COALESCE(excluded.display_name, users.display_name),
                    full_name       = COALESCE(excluded.full_name, users.full_name),
                    remote_id       = COALESCE(excluded.remote_id, users.remote_id),
                    avatar_blob_id  = COALESCE(excluded.avatar_blob_id, users.avatar_blob_id),
                    payload         = excluded.payload",
            );
            let mut q = sqlx::query(&sql);
            for (r, (id, payload_txt)) in chunk.iter().zip(prepared_chunk.iter()) {
                q = q
                    .bind(id)
                    .bind(&r.source)
                    .bind(r.network.as_deref())
                    .bind(&r.native_user_id)
                    .bind(r.display_name.as_deref())
                    .bind(r.full_name.as_deref())
                    .bind(r.remote_id.as_deref())
                    .bind(r.avatar_blob_id.as_deref())
                    .bind(payload_txt);
            }
            q.execute(&mut *tx).await.context("bulk insert users")?;
        }
        bulk_upsert_bookkeeping(
            &mut tx,
            "users",
            prepared.iter().map(|(id, _)| id.as_str()),
            &now,
        )
        .await?;
        tx.commit().await.context("commit bulk users tx")?;
        Ok(())
    }

    /// Bulk-upsert events via chunked multi-row INSERT inside one tx.
    /// Hot path on a fresh ingest (thousands to tens of thousands of
    /// events per beeper account). Synthesized PKs and FK uuids are
    /// computed eagerly per row.
    pub async fn bulk_upsert_events(&self, rows: &[EventRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        struct Prepared {
            id: String,
            room_uuid: String,
            sender_uuid: Option<String>,
            payload_txt: String,
        }
        let prepared: Vec<Prepared> = rows
            .iter()
            .map(|r| Prepared {
                id: beeper_event_uuid(&r.source, &r.native_event_id),
                room_uuid: beeper_room_uuid(&r.source, &r.native_room_id),
                sender_uuid: r
                    .sender_native_user_id
                    .as_deref()
                    .map(|s| beeper_user_uuid(&r.source, s)),
                payload_txt: serde_json::to_string(&r.payload)
                    .unwrap_or_else(|_| "null".to_string()),
            })
            .collect();
        let mut tx = self.pool.begin().await.context("begin bulk events tx")?;
        for (chunk, prepared_chunk) in rows.chunks(SQL_CHUNK).zip(prepared.chunks(SQL_CHUNK)) {
            let mut sql = String::from(
                "INSERT INTO events (
                    id, source, network, room_uuid, sender_uuid,
                    native_event_id, external_event_id,
                    event_type, timestamp_ms, text_content,
                    reply_to_native_event_id, edit_of_native_event_id,
                    reaction_emoji, reaction_target_native_event_id,
                    payload
                ) VALUES ",
            );
            for i in 0..chunk.len() {
                if i > 0 {
                    sql.push(',');
                }
                sql.push_str("(?,?,?,?,?,?,?,?,?,?,?,?,?,?,jsonb(?))");
            }
            sql.push_str(
                " ON CONFLICT(id) DO UPDATE SET
                    sender_uuid                       = COALESCE(excluded.sender_uuid, events.sender_uuid),
                    external_event_id                 = COALESCE(excluded.external_event_id, events.external_event_id),
                    event_type                        = excluded.event_type,
                    timestamp_ms                      = excluded.timestamp_ms,
                    text_content                      = excluded.text_content,
                    reply_to_native_event_id          = COALESCE(excluded.reply_to_native_event_id, events.reply_to_native_event_id),
                    edit_of_native_event_id           = COALESCE(excluded.edit_of_native_event_id, events.edit_of_native_event_id),
                    reaction_emoji                    = COALESCE(excluded.reaction_emoji, events.reaction_emoji),
                    reaction_target_native_event_id   = COALESCE(excluded.reaction_target_native_event_id, events.reaction_target_native_event_id),
                    payload                           = excluded.payload",
            );
            let mut q = sqlx::query(&sql);
            for (r, p) in chunk.iter().zip(prepared_chunk.iter()) {
                q = q
                    .bind(&p.id)
                    .bind(&r.source)
                    .bind(&r.network)
                    .bind(&p.room_uuid)
                    .bind(p.sender_uuid.as_deref())
                    .bind(&r.native_event_id)
                    .bind(r.external_event_id.as_deref())
                    .bind(&r.event_type)
                    .bind(r.timestamp_ms)
                    .bind(r.text_content.as_deref())
                    .bind(r.reply_to_native_event_id.as_deref())
                    .bind(r.edit_of_native_event_id.as_deref())
                    .bind(r.reaction_emoji.as_deref())
                    .bind(r.reaction_target_native_event_id.as_deref())
                    .bind(&p.payload_txt);
            }
            q.execute(&mut *tx).await.context("bulk insert events")?;
        }
        bulk_upsert_bookkeeping(
            &mut tx,
            "events",
            prepared.iter().map(|p| p.id.as_str()),
            &now,
        )
        .await?;
        tx.commit().await.context("commit bulk events tx")?;
        Ok(())
    }
}
