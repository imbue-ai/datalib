//! Doltlite-backed raw store for the Beeper provider.
//!
//! Single sqlite file at `<data_root>/raw/<name>.doltlite_db`. Shared
//! bookkeeping tables (`blobs`, `endpoint_shapes`, `sync_runs`) and
//! the open / blob plumbing live in [`frankweiler_etl::doltlite_raw`];
//! the primary-key policy is documented there.
//!
//! Tables:
//! - `rooms` — PK is `beeper_room_uuid(matrix_room_id)`. The Matrix
//!   room id (`!abc:beeper.com`) IS a stable upstream identifier, but
//!   it contains characters (`!`, `:`) that some downstream tools
//!   choke on as a primary key; the v5 uuid keeps `dolt diff`
//!   readable and matches the slack/notion convention. The Matrix
//!   room id lives alongside as its own column so cross-table queries
//!   don't have to reverse the hash.
//! - `users` — PK is `beeper_user_uuid(matrix_user_id)`. Same logic.
//!   A bridge-side native identifier (the iMessage handle, WhatsApp
//!   phone number, etc.) is parsed out of the mxid localpart and
//!   stored as `remote_id` when recognisable.
//! - `events` — PK is `beeper_event_uuid(matrix_room_id, event_id)`.
//!   Matrix event ids ARE globally unique (`$xyz`), but namespacing
//!   the uuid by room keeps it impossible to accidentally collide
//!   events across rooms during a future schema migration.
//! - `sync_cursors` — bookkeeping. PK is `matrix_room_id`; carries
//!   the `prev_batch` token from the last `/messages?dir=b` walk so
//!   the next backfill resumes there. Milestone A doesn't populate
//!   this yet — kept here so the schema doesn't churn when
//!   Milestone B lands.

use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::Value;
use sqlx::sqlite::SqlitePool;

use frankweiler_etl::doltlite_raw::{self as dr};

pub use frankweiler_etl::doltlite_raw::db_path_for;

use crate::translate::{beeper_room_uuid, beeper_user_uuid};

use super::RoomInfo;

/// Scope key under which we cache the global Matrix `/sync` cursor
/// (`next_batch` from the most recent successful sync) in the shared
/// `sync_scope_state` table. Subsequent runs pass it as `since=` to
/// drop into incremental-sync mode.
pub const SYNC_SCOPE_NEXT_BATCH: &str = "matrix_sync_next_batch";

const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS rooms (
        id TEXT PRIMARY KEY,
        matrix_room_id TEXT NOT NULL,
        bridge_network TEXT NOT NULL,
        bridge_protocol TEXT NULL,
        display_name TEXT NULL,
        topic TEXT NULL,
        is_dm INTEGER NOT NULL DEFAULT 0,
        is_space INTEGER NOT NULL DEFAULT 0,
        payload TEXT NULL,
        fetched_at TEXT NULL,
        attempt_count INTEGER NOT NULL DEFAULT 0,
        last_attempt_at TEXT NULL,
        last_error TEXT NULL
    )",
    "CREATE UNIQUE INDEX IF NOT EXISTS rooms_by_matrix_id ON rooms(matrix_room_id)",
    "CREATE INDEX IF NOT EXISTS rooms_by_network ON rooms(bridge_network)",
    "CREATE TABLE IF NOT EXISTS users (
        id TEXT PRIMARY KEY,
        matrix_user_id TEXT NOT NULL,
        bridge_network TEXT NULL,
        remote_id TEXT NULL,
        display_name TEXT NULL,
        avatar_mxc TEXT NULL,
        payload TEXT NULL,
        fetched_at TEXT NULL,
        attempt_count INTEGER NOT NULL DEFAULT 0,
        last_attempt_at TEXT NULL,
        last_error TEXT NULL
    )",
    "CREATE UNIQUE INDEX IF NOT EXISTS users_by_matrix_id ON users(matrix_user_id)",
    "CREATE TABLE IF NOT EXISTS events (
        id TEXT PRIMARY KEY,
        matrix_event_id TEXT NOT NULL,
        matrix_room_id TEXT NOT NULL,
        room_uuid TEXT NOT NULL,
        sender_mxid TEXT NULL,
        sender_uuid TEXT NULL,
        origin_ts INTEGER NULL,
        event_type TEXT NULL,
        msgtype TEXT NULL,
        relates_to TEXT NULL,
        body TEXT NULL,
        payload TEXT NULL,
        fetched_at TEXT NULL,
        attempt_count INTEGER NOT NULL DEFAULT 0,
        last_attempt_at TEXT NULL,
        last_error TEXT NULL
    )",
    "CREATE INDEX IF NOT EXISTS events_by_room_ts ON events(matrix_room_id, origin_ts)",
    "CREATE TABLE IF NOT EXISTS sync_cursors (
        matrix_room_id TEXT PRIMARY KEY,
        prev_batch TEXT NULL,
        last_event_id TEXT NULL,
        fetched_at TEXT NULL
    )",
];

#[derive(Clone)]
pub struct RawDb {
    pool: SqlitePool,
}

impl RawDb {
    pub async fn open(db_path: &Path) -> Result<Self> {
        let pool = dr::open(db_path, DDL).await?;
        Ok(Self { pool })
    }

    pub async fn start_run(&self, config: &Value) -> Result<i64> {
        dr::start_run(&self.pool, config).await
    }

    pub async fn finish_run(&self, run_id: i64, status: &str, summary: &Value) -> Result<()> {
        dr::finish_run(&self.pool, run_id, status, summary).await
    }

    /// Upsert a room. Stomps the `payload` column with the latest
    /// `/state` response so re-runs reflect renames / topic edits.
    pub async fn upsert_room(&self, info: &RoomInfo, state_payload: &Value) -> Result<()> {
        let id = beeper_room_uuid(&info.matrix_room_id);
        let now = Utc::now().to_rfc3339();
        let payload = serde_json::to_string(state_payload).context("serialize room payload")?;
        sqlx::query(
            "INSERT INTO rooms (
                id, matrix_room_id, bridge_network, bridge_protocol,
                display_name, topic, is_dm, is_space,
                payload, fetched_at, attempt_count, last_attempt_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, jsonb(?), ?, 1, ?)
            ON CONFLICT(id) DO UPDATE SET
                bridge_network  = excluded.bridge_network,
                bridge_protocol = excluded.bridge_protocol,
                display_name    = excluded.display_name,
                topic           = excluded.topic,
                is_dm           = excluded.is_dm,
                is_space        = excluded.is_space,
                payload         = excluded.payload,
                fetched_at      = excluded.fetched_at,
                attempt_count   = rooms.attempt_count + 1,
                last_attempt_at = excluded.last_attempt_at",
        )
        .bind(&id)
        .bind(&info.matrix_room_id)
        .bind(&info.bridge_network)
        .bind(info.bridge_protocol.as_deref())
        .bind(info.display_name.as_deref())
        .bind(info.topic.as_deref())
        .bind(info.is_dm as i64)
        .bind(info.is_space as i64)
        .bind(&payload)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await
        .context("insert rooms")?;
        Ok(())
    }

    /// Upsert a user. `network_hint` is used when we already know the
    /// bridge (e.g. caller scraped the room's `m.bridge` state event);
    /// otherwise we'll fall back to a localpart sniff at translate
    /// time. Payload is the most recent `m.room.member` event for this
    /// user, or `whoami` for the self row.
    /// Upsert one timeline / state event row. `room_info` provides the
    /// already-derived `bridge_network` so we don't have to re-sniff
    /// per-event.
    pub async fn upsert_event(&self, matrix_room_id: &str, event: &Value) -> Result<()> {
        let Some(event_id) = event.get("event_id").and_then(|v| v.as_str()) else {
            return Ok(());
        };
        let id =
            crate::translate::beeper_event_uuid(matrix_room_id, event_id);
        let room_uuid = crate::translate::beeper_room_uuid(matrix_room_id);
        let sender_mxid = event
            .get("sender")
            .and_then(|v| v.as_str())
            .map(String::from);
        let sender_uuid = sender_mxid
            .as_deref()
            .map(crate::translate::beeper_user_uuid);
        let origin_ts = event.get("origin_server_ts").and_then(|v| v.as_i64());
        let event_type = event
            .get("type")
            .and_then(|v| v.as_str())
            .map(String::from);
        let msgtype = event
            .pointer("/content/msgtype")
            .and_then(|v| v.as_str())
            .map(String::from);
        let relates_to = event
            .pointer("/content/m.relates_to/event_id")
            .or_else(|| event.pointer("/content/m\\.relates_to/event_id"))
            .and_then(|v| v.as_str())
            .map(String::from);
        let body = event
            .pointer("/content/body")
            .and_then(|v| v.as_str())
            .map(String::from);
        let payload_str = serde_json::to_string(event).context("serialize event payload")?;
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO events (
                id, matrix_event_id, matrix_room_id, room_uuid,
                sender_mxid, sender_uuid, origin_ts,
                event_type, msgtype, relates_to, body,
                payload, fetched_at, attempt_count, last_attempt_at
            ) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, jsonb(?), ?, 1, ?)
            ON CONFLICT(id) DO UPDATE SET
                sender_mxid     = COALESCE(excluded.sender_mxid, events.sender_mxid),
                sender_uuid     = COALESCE(excluded.sender_uuid, events.sender_uuid),
                origin_ts       = COALESCE(excluded.origin_ts, events.origin_ts),
                event_type      = COALESCE(excluded.event_type, events.event_type),
                msgtype         = COALESCE(excluded.msgtype, events.msgtype),
                relates_to      = COALESCE(excluded.relates_to, events.relates_to),
                body            = COALESCE(excluded.body, events.body),
                payload         = excluded.payload,
                fetched_at      = excluded.fetched_at,
                attempt_count   = events.attempt_count + 1,
                last_attempt_at = excluded.last_attempt_at",
        )
        .bind(&id)
        .bind(event_id)
        .bind(matrix_room_id)
        .bind(&room_uuid)
        .bind(sender_mxid.as_deref())
        .bind(sender_uuid.as_deref())
        .bind(origin_ts)
        .bind(event_type.as_deref())
        .bind(msgtype.as_deref())
        .bind(relates_to.as_deref())
        .bind(body.as_deref())
        .bind(&payload_str)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await
        .context("insert events")?;
        Ok(())
    }

    /// Record a room's `prev_batch` token from the most recent sync's
    /// timeline chunk. Milestone B's backfill loop pages backward from
    /// here via `/messages?dir=b&from=<prev_batch>`.
    pub async fn upsert_sync_cursor(
        &self,
        matrix_room_id: &str,
        prev_batch: Option<&str>,
        last_event_id: Option<&str>,
    ) -> Result<()> {
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO sync_cursors (matrix_room_id, prev_batch, last_event_id, fetched_at)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(matrix_room_id) DO UPDATE SET
                prev_batch    = COALESCE(excluded.prev_batch, sync_cursors.prev_batch),
                last_event_id = COALESCE(excluded.last_event_id, sync_cursors.last_event_id),
                fetched_at    = excluded.fetched_at",
        )
        .bind(matrix_room_id)
        .bind(prev_batch)
        .bind(last_event_id)
        .bind(&now)
        .execute(&self.pool)
        .await
        .context("insert sync_cursors")?;
        Ok(())
    }

    /// Read the global `next_batch` cursor from the previous successful
    /// sync, if any. Stored in the shared `sync_scope_state` table.
    pub async fn read_next_batch(&self) -> Result<Option<String>> {
        let row: Option<(String,)> = sqlx::query_as(
            "SELECT last_seen_at FROM sync_scope_state WHERE scope = ?",
        )
        .bind(SYNC_SCOPE_NEXT_BATCH)
        .fetch_optional(&self.pool)
        .await
        .context("read sync_scope_state")?;
        Ok(row.map(|(v,)| v))
    }

    /// Persist the global `next_batch` cursor returned by Matrix
    /// `/sync`. Subsequent runs pass it as `since=` for incremental sync.
    pub async fn write_next_batch(&self, next_batch: &str) -> Result<()> {
        sqlx::query(
            "INSERT INTO sync_scope_state (scope, last_seen_at) VALUES (?, ?)
             ON CONFLICT(scope) DO UPDATE SET last_seen_at = excluded.last_seen_at",
        )
        .bind(SYNC_SCOPE_NEXT_BATCH)
        .bind(next_batch)
        .execute(&self.pool)
        .await
        .context("update sync_scope_state")?;
        Ok(())
    }

    pub async fn upsert_user(
        &self,
        matrix_user_id: &str,
        network_hint: Option<&str>,
        payload: &Value,
    ) -> Result<()> {
        let id = beeper_user_uuid(matrix_user_id);
        let now = Utc::now().to_rfc3339();
        let display_name = payload
            .pointer("/content/displayname")
            .and_then(|v| v.as_str())
            .map(String::from);
        let avatar_mxc = payload
            .pointer("/content/avatar_url")
            .and_then(|v| v.as_str())
            .map(String::from);
        let remote_id = remote_id_from_mxid(matrix_user_id);
        let payload_str = serde_json::to_string(payload).context("serialize user payload")?;
        sqlx::query(
            "INSERT INTO users (
                id, matrix_user_id, bridge_network, remote_id,
                display_name, avatar_mxc,
                payload, fetched_at, attempt_count, last_attempt_at
            ) VALUES (?, ?, ?, ?, ?, ?, jsonb(?), ?, 1, ?)
            ON CONFLICT(id) DO UPDATE SET
                bridge_network  = COALESCE(excluded.bridge_network, users.bridge_network),
                remote_id       = COALESCE(excluded.remote_id, users.remote_id),
                display_name    = COALESCE(excluded.display_name, users.display_name),
                avatar_mxc      = COALESCE(excluded.avatar_mxc, users.avatar_mxc),
                payload         = excluded.payload,
                fetched_at      = excluded.fetched_at,
                attempt_count   = users.attempt_count + 1,
                last_attempt_at = excluded.last_attempt_at",
        )
        .bind(&id)
        .bind(matrix_user_id)
        .bind(network_hint)
        .bind(remote_id.as_deref())
        .bind(display_name.as_deref())
        .bind(avatar_mxc.as_deref())
        .bind(&payload_str)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await
        .context("insert users")?;
        Ok(())
    }
}

/// Parse the bridge-side native identifier out of a Beeper-issued mxid
/// localpart. E.g. `@signal_+15551234567:beeper.local` → `+15551234567`.
/// Returns `None` for self / Matrix-native users and for bridge bots.
fn remote_id_from_mxid(mxid: &str) -> Option<String> {
    let local = mxid.strip_prefix('@')?.split(':').next()?;
    // bridge users follow `<network>_<id>` (signal, telegram, discord, …)
    // or `<network>bot` for the bridge bot itself (no remote id).
    if local.ends_with("bot") {
        return None;
    }
    let (_net, id) = local.split_once('_')?;
    if id.is_empty() {
        None
    } else {
        Some(id.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_id_extracts_signal_phone() {
        assert_eq!(
            remote_id_from_mxid("@signal_+15551234567:beeper.local"),
            Some("+15551234567".to_string())
        );
    }

    #[test]
    fn remote_id_skips_bridge_bot() {
        assert_eq!(remote_id_from_mxid("@signalbot:beeper.local"), None);
    }

    #[test]
    fn remote_id_skips_matrix_user() {
        assert_eq!(remote_id_from_mxid("@thad:beeper.com"), None);
    }
}
