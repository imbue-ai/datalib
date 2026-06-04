//! Doltlite-backed raw store for the Beeper provider.
//!
//! Single sqlite file at `<data_root>/raw/<name>.doltlite_db`. Shared
//! bookkeeping tables (`blobs`, `endpoint_shapes`, `sync_runs`) and
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

use frankweiler_etl::doltlite_raw::{self as dr};

pub use frankweiler_etl::doltlite_raw::db_path_for;

use crate::translate::{beeper_event_uuid, beeper_room_uuid, beeper_user_uuid};

/// Data tables — what `dolt diff` should see across re-fetches.
/// Bookkeeping columns live in `<table>_bookkeeping` sidecars added
/// via `dr::bookkeeping_ddl_for(...)` below.
const DATA_TABLES: &[&str] = &["rooms", "users", "events"];

const DDL_DATA: &[&str] = &[
    // Native vs external IDs:
    //
    //   * `native_room_id` is the room's identifier inside Beeper's
    //     universe (the Matrix room id, e.g. `!abc:beeper.local`).
    //     This is what every Beeper-internal reference resolves
    //     through, and it's what we use to derive our v5 UUIDs.
    //   * `external_room_id` / `external_workspace_id` are the
    //     UPSTREAM system's canonical IDs (the Signal conversation
    //     UUID, the Slack channel id, the Google Chat space id, …).
    //     These are the IDs you'd use to talk to the underlying
    //     service's own API. Sourced from `thread.extra.bridge.*`
    //     when Beeper populates them.
    "CREATE TABLE IF NOT EXISTS rooms (
        id TEXT PRIMARY KEY,
        source TEXT NOT NULL,
        network TEXT NOT NULL,
        native_room_id TEXT NOT NULL,
        external_room_id TEXT NULL,
        external_workspace_id TEXT NULL,
        account_id TEXT NULL,
        room_type TEXT NULL,
        title TEXT NULL,
        description TEXT NULL,
        is_dm INTEGER NOT NULL DEFAULT 0,
        is_space INTEGER NOT NULL DEFAULT 0,
        payload TEXT NULL
    )",
    "CREATE UNIQUE INDEX IF NOT EXISTS rooms_by_source_native ON rooms(source, native_room_id)",
    "CREATE INDEX IF NOT EXISTS rooms_by_network ON rooms(network)",
    "CREATE TABLE IF NOT EXISTS users (
        id TEXT PRIMARY KEY,
        source TEXT NOT NULL,
        network TEXT NULL,
        native_user_id TEXT NOT NULL,
        display_name TEXT NULL,
        full_name TEXT NULL,
        remote_id TEXT NULL,
        avatar_blob_id TEXT NULL,
        payload TEXT NULL
    )",
    "CREATE UNIQUE INDEX IF NOT EXISTS users_by_source_native ON users(source, native_user_id)",
    // `external_event_id`: the upstream system's canonical message
    // id (Signal message UUID, Slack ts, etc.). NOT populated from
    // index.db — Beeper doesn't propagate these into the desktop
    // app's cache. Filled in by future readers that crack open
    // `local-<bridge>/megabridge.db` (for local bridges) or query
    // Beeper Cloud (for cloud bridges). Column reserved here so
    // the schema doesn't churn when it lands.
    "CREATE TABLE IF NOT EXISTS events (
        id TEXT PRIMARY KEY,
        source TEXT NOT NULL,
        network TEXT NOT NULL,
        room_uuid TEXT NOT NULL,
        sender_uuid TEXT NULL,
        native_event_id TEXT NOT NULL,
        external_event_id TEXT NULL,
        event_type TEXT NOT NULL,
        timestamp_ms INTEGER NOT NULL,
        text_content TEXT NULL,
        reply_to_native_event_id TEXT NULL,
        edit_of_native_event_id TEXT NULL,
        reaction_emoji TEXT NULL,
        reaction_target_native_event_id TEXT NULL,
        payload TEXT NULL
    )",
    "CREATE INDEX IF NOT EXISTS events_by_room_ts ON events(room_uuid, timestamp_ms)",
    "CREATE INDEX IF NOT EXISTS events_by_source_native ON events(source, native_event_id)",
];

fn full_ddl() -> Vec<String> {
    let mut out: Vec<String> = DDL_DATA.iter().map(|s| (*s).to_string()).collect();
    for table in DATA_TABLES {
        out.push(dr::bookkeeping_ddl_for(table));
    }
    out
}

#[derive(Clone, Debug)]
pub struct RawDb {
    pool: SqlitePool,
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
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// Wipe every per-row table so the next fetch re-downloads
    /// everything from upstream. See
    /// [`frankweiler_etl::doltlite_raw::truncate_data_tables`].
    pub async fn reset(&self) -> Result<()> {
        dr::truncate_data_tables(&self.pool, DATA_TABLES).await
    }

    pub async fn start_run(&self, config: &Value) -> Result<i64> {
        dr::start_run(&self.pool, config).await
    }

    pub async fn finish_run(&self, run_id: i64, status: &str, summary: &Value) -> Result<()> {
        dr::finish_run(&self.pool, run_id, status, summary).await
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
        let blobs = sqlx::query("SELECT COUNT(*) AS n FROM blobs WHERE bytes IS NOT NULL")
            .fetch_one(&self.pool)
            .await
            .context("count blobs with bytes")?
            .try_get::<i64, _>("n")
            .unwrap_or(0) as usize;
        // `last_error` now lives on the bookkeeping sidecar — join in.
        let blob_errors =
            sqlx::query("SELECT COUNT(*) AS n FROM blobs_bookkeeping WHERE last_error IS NOT NULL")
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

    pub async fn upsert_room(&self, row: &RoomRow) -> Result<String> {
        let id = beeper_room_uuid(&row.source, &row.native_room_id);
        let payload = serde_json::to_string(&row.payload).context("serialize room payload")?;
        let mut tx = self.pool.begin().await.context("begin room tx")?;
        sqlx::query(
            "INSERT INTO rooms (
                id, source, network, native_room_id,
                external_room_id, external_workspace_id,
                account_id, room_type, title, description, is_dm, is_space,
                payload
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, jsonb(?))
            ON CONFLICT(id) DO UPDATE SET
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
        )
        .bind(&id)
        .bind(&row.source)
        .bind(&row.network)
        .bind(&row.native_room_id)
        .bind(row.external_room_id.as_deref())
        .bind(row.external_workspace_id.as_deref())
        .bind(row.account_id.as_deref())
        .bind(row.room_type.as_deref())
        .bind(row.title.as_deref())
        .bind(row.description.as_deref())
        .bind(row.is_dm as i64)
        .bind(row.is_space as i64)
        .bind(&payload)
        .execute(&mut *tx)
        .await
        .context("insert rooms")?;
        dr::record_object_attempt(&mut tx, "rooms", &id, None).await?;
        tx.commit().await.context("commit room tx")?;
        Ok(id)
    }

    pub async fn upsert_user(&self, row: &UserRow) -> Result<String> {
        let id = beeper_user_uuid(&row.source, &row.native_user_id);
        let payload = serde_json::to_string(&row.payload).context("serialize user payload")?;
        let mut tx = self.pool.begin().await.context("begin user tx")?;
        sqlx::query(
            "INSERT INTO users (
                id, source, network, native_user_id,
                display_name, full_name, remote_id, avatar_blob_id,
                payload
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, jsonb(?))
            ON CONFLICT(id) DO UPDATE SET
                network         = COALESCE(excluded.network, users.network),
                display_name    = COALESCE(excluded.display_name, users.display_name),
                full_name       = COALESCE(excluded.full_name, users.full_name),
                remote_id       = COALESCE(excluded.remote_id, users.remote_id),
                avatar_blob_id  = COALESCE(excluded.avatar_blob_id, users.avatar_blob_id),
                payload         = excluded.payload",
        )
        .bind(&id)
        .bind(&row.source)
        .bind(row.network.as_deref())
        .bind(&row.native_user_id)
        .bind(row.display_name.as_deref())
        .bind(row.full_name.as_deref())
        .bind(row.remote_id.as_deref())
        .bind(row.avatar_blob_id.as_deref())
        .bind(&payload)
        .execute(&mut *tx)
        .await
        .context("insert users")?;
        dr::record_object_attempt(&mut tx, "users", &id, None).await?;
        tx.commit().await.context("commit user tx")?;
        Ok(id)
    }

    pub async fn upsert_event(&self, row: &EventRow) -> Result<String> {
        let id = beeper_event_uuid(&row.source, &row.native_event_id);
        let room_uuid = beeper_room_uuid(&row.source, &row.native_room_id);
        let sender_uuid = row
            .sender_native_user_id
            .as_deref()
            .map(|s| beeper_user_uuid(&row.source, s));
        let payload = serde_json::to_string(&row.payload).context("serialize event payload")?;
        let mut tx = self.pool.begin().await.context("begin event tx")?;
        sqlx::query(
            "INSERT INTO events (
                id, source, network, room_uuid, sender_uuid,
                native_event_id, external_event_id,
                event_type, timestamp_ms, text_content,
                reply_to_native_event_id, edit_of_native_event_id,
                reaction_emoji, reaction_target_native_event_id,
                payload
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, jsonb(?))
            ON CONFLICT(id) DO UPDATE SET
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
        )
        .bind(&id)
        .bind(&row.source)
        .bind(&row.network)
        .bind(&room_uuid)
        .bind(sender_uuid.as_deref())
        .bind(&row.native_event_id)
        .bind(row.external_event_id.as_deref())
        .bind(&row.event_type)
        .bind(row.timestamp_ms)
        .bind(row.text_content.as_deref())
        .bind(row.reply_to_native_event_id.as_deref())
        .bind(row.edit_of_native_event_id.as_deref())
        .bind(row.reaction_emoji.as_deref())
        .bind(row.reaction_target_native_event_id.as_deref())
        .bind(&payload)
        .execute(&mut *tx)
        .await
        .context("insert events")?;
        dr::record_object_attempt(&mut tx, "events", &id, None).await?;
        tx.commit().await.context("commit event tx")?;
        Ok(id)
    }
}
