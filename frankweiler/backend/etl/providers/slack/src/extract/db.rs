//! Doltlite-backed raw store for the Slack provider.
//!
//! Replaces the JSONL tree of `raw_api/<method>/run-*.jsonl` with a
//! single sqlite file at `<data_root>/raw/<name>.doltlite_db`. Shared
//! bookkeeping tables (`blobs`, `endpoint_shapes`, `sync_runs`) and the
//! open / blob plumbing live in [`frankweiler_etl::doltlite_raw`]; the
//! primary-key policy that governs every object table here is
//! documented there.
//!
//! Tables:
//! - `workspaces` — PK is upstream `team_id` from `auth.test`.
//! - `users` — PK is upstream `user_id`.
//! - `channels` — PK is upstream `channel_id`.
//! - `messages` — PK is `slack_message_uuid(team_id, channel_id, ts)`,
//!   a UUIDv5 derived from the three Slack-supplied identifiers. The
//!   guide's default is a literal upstream id, but Slack messages have
//!   no single string id upstream — `ts` is unique only within a
//!   `(team, channel)` scope. Deriving the PK from those three columns
//!   keeps it stably derived from upstream data (a wipe-and-reingest
//!   yields the same uuid byte-for-byte) and still known before fetch
//!   (history pages supply `ts`; the `team_id` we cache from
//!   `auth.test`; the channel from the listing). The three components
//!   are stored as their own columns alongside `id` so cross-table
//!   queries don't have to reverse the v5 hash.
//! - `replies_pages` — bookkeeping row per `(channel_id, thread_ts)` we
//!   have a `conversations.replies` capture for. Lets us track which
//!   threads we've walked + when, without folding it into the
//!   `messages` table. Replies' bodies land in `messages` like history
//!   messages do.

use std::collections::HashMap;
use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::Value;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use frankweiler_etl::doltlite_raw::{self as dr};

pub use frankweiler_etl::doltlite_raw::{db_path_for, BlobBytes};

use crate::translate::{slack_message_uuid, slack_thread_uuid};

const DDL: &[&str] = &[
    "CREATE TABLE IF NOT EXISTS workspaces (
        id TEXT PRIMARY KEY,
        team_name TEXT NULL,
        team_url TEXT NULL,
        self_user_id TEXT NULL,
        payload TEXT NULL,
        fetched_at TEXT NULL,
        attempt_count INTEGER NOT NULL DEFAULT 0,
        last_attempt_at TEXT NULL,
        last_error TEXT NULL
    )",
    "CREATE TABLE IF NOT EXISTS users (
        id TEXT PRIMARY KEY,
        team_id TEXT NULL,
        name TEXT NULL,
        real_name TEXT NULL,
        display_name TEXT NULL,
        payload TEXT NULL,
        fetched_at TEXT NULL,
        attempt_count INTEGER NOT NULL DEFAULT 0,
        last_attempt_at TEXT NULL,
        last_error TEXT NULL
    )",
    "CREATE TABLE IF NOT EXISTS channels (
        id TEXT PRIMARY KEY,
        name TEXT NULL,
        is_member INTEGER NULL,
        is_archived INTEGER NULL,
        payload TEXT NULL,
        fetched_at TEXT NULL,
        attempt_count INTEGER NOT NULL DEFAULT 0,
        last_attempt_at TEXT NULL,
        last_error TEXT NULL
    )",
    "CREATE TABLE IF NOT EXISTS messages (
        id TEXT PRIMARY KEY,
        team_id TEXT NOT NULL,
        channel_id TEXT NOT NULL,
        ts TEXT NOT NULL,
        thread_ts TEXT NULL,
        thread_root_uuid TEXT NULL,
        is_thread_root INTEGER NULL,
        user_id TEXT NULL,
        payload TEXT NULL,
        fetched_at TEXT NULL,
        attempt_count INTEGER NOT NULL DEFAULT 0,
        last_attempt_at TEXT NULL,
        last_error TEXT NULL
    )",
    "CREATE INDEX IF NOT EXISTS messages_by_channel_ts ON messages(channel_id, ts)",
    "CREATE INDEX IF NOT EXISTS messages_by_thread ON messages(thread_root_uuid)",
    "CREATE TABLE IF NOT EXISTS replies_pages (
        id TEXT PRIMARY KEY,
        channel_id TEXT NOT NULL,
        thread_ts TEXT NOT NULL,
        latest_reply TEXT NULL,
        fetched_at TEXT NULL,
        attempt_count INTEGER NOT NULL DEFAULT 0,
        last_attempt_at TEXT NULL,
        last_error TEXT NULL
    )",
];

#[derive(Clone)]
pub struct RawDb {
    pool: SqlitePool,
}

/// One row of input for [`RawDb::upsert_message`]. Carries the upstream
/// JSON body plus the columns we promote from it for cheap predicate
/// queries.
#[derive(Debug, Clone)]
pub struct MessageRow {
    pub team_id: String,
    pub channel_id: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub is_thread_root: bool,
    pub user_id: Option<String>,
    /// Raw Slack message JSON, byte-for-byte.
    pub payload: Value,
}

impl RawDb {
    pub async fn open(db_path: &Path) -> Result<Self> {
        let pool = dr::open(db_path, DDL).await?;
        Ok(Self { pool })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub async fn start_run(&self, config: &Value) -> Result<i64> {
        dr::start_run(&self.pool, config).await
    }

    pub async fn finish_run(&self, run_id: i64, status: &str, summary: &Value) -> Result<()> {
        dr::finish_run(&self.pool, run_id, status, summary).await
    }

    // ── workspace ───────────────────────────────────────────────────

    pub async fn upsert_workspace(&self, payload: &Value) -> Result<()> {
        let team_id = payload
            .get("team_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("auth.test response missing team_id"))?;
        let team_name = payload.get("team").and_then(|v| v.as_str());
        let team_url = payload.get("url").and_then(|v| v.as_str());
        let self_user_id = payload.get("user_id").and_then(|v| v.as_str());
        let payload_str = serde_json::to_string(payload).context("serialize auth.test")?;
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO workspaces
                (id, team_name, team_url, self_user_id, payload, fetched_at, last_attempt_at, last_error)
             VALUES (?, ?, ?, ?, jsonb(?), ?, ?, NULL)
             ON CONFLICT(id) DO UPDATE SET
                team_name = COALESCE(excluded.team_name, workspaces.team_name),
                team_url = COALESCE(excluded.team_url, workspaces.team_url),
                self_user_id = COALESCE(excluded.self_user_id, workspaces.self_user_id),
                payload = excluded.payload,
                fetched_at = excluded.fetched_at,
                last_attempt_at = excluded.last_attempt_at,
                last_error = NULL",
        )
        .bind(team_id)
        .bind(team_name)
        .bind(team_url)
        .bind(self_user_id)
        .bind(&payload_str)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await
        .context("upsert workspace")?;
        Ok(())
    }

    /// Return the cached workspace `team_id` (the most-recently-seen
    /// row) so callers that need it before re-fetching `auth.test`
    /// don't have to walk the payload.
    pub async fn cached_team_id(&self) -> Result<Option<String>> {
        let row = sqlx::query("SELECT id FROM workspaces ORDER BY fetched_at DESC LIMIT 1")
            .fetch_optional(&self.pool)
            .await
            .context("select cached team_id")?;
        Ok(row.and_then(|r| r.try_get::<String, _>("id").ok()))
    }

    pub async fn load_workspace(&self) -> Result<Option<Value>> {
        let row =
            sqlx::query("SELECT json(payload) AS payload FROM workspaces ORDER BY id LIMIT 1")
                .fetch_optional(&self.pool)
                .await
                .context("select workspace")?;
        let Some(row) = row else { return Ok(None) };
        let payload: Option<String> = row.try_get("payload").ok();
        Ok(payload.and_then(|s| serde_json::from_str(&s).ok()))
    }

    // ── users ───────────────────────────────────────────────────────

    pub async fn upsert_user(&self, payload: &Value) -> Result<()> {
        let id = payload
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("user response missing id"))?;
        let team_id = payload.get("team_id").and_then(|v| v.as_str());
        let name = payload.get("name").and_then(|v| v.as_str());
        let real_name = payload
            .get("real_name")
            .and_then(|v| v.as_str())
            .or_else(|| {
                payload
                    .get("profile")
                    .and_then(|p| p.get("real_name"))
                    .and_then(|v| v.as_str())
            });
        let display_name = payload
            .get("profile")
            .and_then(|p| p.get("display_name"))
            .and_then(|v| v.as_str());
        let payload_str = serde_json::to_string(payload).context("serialize user")?;
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO users
                (id, team_id, name, real_name, display_name, payload, fetched_at, last_attempt_at, last_error)
             VALUES (?, ?, ?, ?, ?, jsonb(?), ?, ?, NULL)
             ON CONFLICT(id) DO UPDATE SET
                team_id = COALESCE(excluded.team_id, users.team_id),
                name = COALESCE(excluded.name, users.name),
                real_name = COALESCE(excluded.real_name, users.real_name),
                display_name = COALESCE(excluded.display_name, users.display_name),
                payload = excluded.payload,
                fetched_at = excluded.fetched_at,
                last_attempt_at = excluded.last_attempt_at,
                last_error = NULL",
        )
        .bind(id)
        .bind(team_id)
        .bind(name)
        .bind(real_name)
        .bind(display_name)
        .bind(&payload_str)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await
        .with_context(|| format!("upsert user {id}"))?;
        Ok(())
    }

    pub async fn load_users(&self) -> Result<Vec<Value>> {
        dr::load_payloads(&self.pool, "users").await
    }

    // ── channels ────────────────────────────────────────────────────

    pub async fn upsert_channel(&self, payload: &Value) -> Result<()> {
        let id = payload
            .get("id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow::anyhow!("channel response missing id"))?;
        let name = payload.get("name").and_then(|v| v.as_str());
        let is_member = payload.get("is_member").and_then(|v| v.as_bool());
        let is_archived = payload.get("is_archived").and_then(|v| v.as_bool());
        let payload_str = serde_json::to_string(payload).context("serialize channel")?;
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO channels
                (id, name, is_member, is_archived, payload, fetched_at, last_attempt_at, last_error)
             VALUES (?, ?, ?, ?, jsonb(?), ?, ?, NULL)
             ON CONFLICT(id) DO UPDATE SET
                name = COALESCE(excluded.name, channels.name),
                is_member = COALESCE(excluded.is_member, channels.is_member),
                is_archived = COALESCE(excluded.is_archived, channels.is_archived),
                payload = excluded.payload,
                fetched_at = excluded.fetched_at,
                last_attempt_at = excluded.last_attempt_at,
                last_error = NULL",
        )
        .bind(id)
        .bind(name)
        .bind(is_member.map(|b| b as i64))
        .bind(is_archived.map(|b| b as i64))
        .bind(&payload_str)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await
        .with_context(|| format!("upsert channel {id}"))?;
        Ok(())
    }

    pub async fn load_channels(&self) -> Result<Vec<Value>> {
        dr::load_payloads(&self.pool, "channels").await
    }

    /// Channels we should iterate during a fetch run. Mirrors the old
    /// `members_only` / `include_archived` filters but reads from the
    /// DB rather than holding the listing in memory.
    pub async fn channels_for_fetch(
        &self,
        members_only: bool,
        include_archived: bool,
    ) -> Result<Vec<(String, Option<String>)>> {
        let mut sql = String::from("SELECT id, name FROM channels WHERE payload IS NOT NULL");
        if members_only {
            sql.push_str(" AND is_member = 1");
        }
        if !include_archived {
            sql.push_str(" AND (is_archived IS NULL OR is_archived = 0)");
        }
        sql.push_str(" ORDER BY id");
        let rows = sqlx::query(&sql)
            .fetch_all(&self.pool)
            .await
            .context("select channels_for_fetch")?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let id: String = r.try_get("id").ok()?;
                let name: Option<String> = r.try_get("name").ok();
                Some((id, name))
            })
            .collect())
    }

    // ── messages ────────────────────────────────────────────────────

    pub async fn upsert_message(&self, row: &MessageRow) -> Result<()> {
        let id = slack_message_uuid(&row.team_id, &row.channel_id, &row.ts);
        let thread_root_uuid = row
            .thread_ts
            .as_ref()
            .map(|tts| slack_thread_uuid(&row.team_id, &row.channel_id, tts));
        let payload_str = serde_json::to_string(&row.payload).context("serialize message")?;
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO messages
                (id, team_id, channel_id, ts, thread_ts, thread_root_uuid, is_thread_root,
                 user_id, payload, fetched_at, last_attempt_at, last_error)
             VALUES (?, ?, ?, ?, ?, ?, ?, ?, jsonb(?), ?, ?, NULL)
             ON CONFLICT(id) DO UPDATE SET
                team_id = excluded.team_id,
                channel_id = excluded.channel_id,
                ts = excluded.ts,
                thread_ts = COALESCE(excluded.thread_ts, messages.thread_ts),
                thread_root_uuid = COALESCE(excluded.thread_root_uuid, messages.thread_root_uuid),
                is_thread_root = COALESCE(excluded.is_thread_root, messages.is_thread_root),
                user_id = COALESCE(excluded.user_id, messages.user_id),
                payload = excluded.payload,
                fetched_at = excluded.fetched_at,
                last_attempt_at = excluded.last_attempt_at,
                last_error = NULL",
        )
        .bind(&id)
        .bind(&row.team_id)
        .bind(&row.channel_id)
        .bind(&row.ts)
        .bind(row.thread_ts.as_deref())
        .bind(thread_root_uuid.as_deref())
        .bind(row.is_thread_root as i64)
        .bind(row.user_id.as_deref())
        .bind(&payload_str)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await
        .with_context(|| format!("upsert message {id}"))?;
        Ok(())
    }

    /// All persisted messages, in `(channel_id, ts)` order so the
    /// downstream translate path produces stable output.
    pub async fn load_messages(&self) -> Result<Vec<LoadedMessage>> {
        let rows = sqlx::query(
            "SELECT id, team_id, channel_id, ts, thread_ts, is_thread_root, user_id,
                    json(payload) AS payload
             FROM messages
             WHERE payload IS NOT NULL
             ORDER BY channel_id, ts",
        )
        .fetch_all(&self.pool)
        .await
        .context("select messages")?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let payload_str: String = match r.try_get("payload") {
                Ok(s) => s,
                Err(_) => continue,
            };
            let Ok(payload) = serde_json::from_str::<Value>(&payload_str) else {
                continue;
            };
            let is_root_int: Option<i64> = r.try_get("is_thread_root").unwrap_or(None);
            out.push(LoadedMessage {
                id: r.try_get("id").unwrap_or_default(),
                team_id: r.try_get("team_id").unwrap_or_default(),
                channel_id: r.try_get("channel_id").unwrap_or_default(),
                ts: r.try_get("ts").unwrap_or_default(),
                thread_ts: r.try_get::<Option<String>, _>("thread_ts").unwrap_or(None),
                is_thread_root: is_root_int.unwrap_or(0) != 0,
                user_id: r.try_get::<Option<String>, _>("user_id").unwrap_or(None),
                payload,
            });
        }
        Ok(out)
    }

    /// `max(ts)` per channel — drives the live downloader's resume
    /// cursor (next history forward pass starts at this ts).
    pub async fn latest_ts_by_channel(&self) -> Result<HashMap<String, String>> {
        let rows =
            sqlx::query("SELECT channel_id, MAX(ts) AS max_ts FROM messages GROUP BY channel_id")
                .fetch_all(&self.pool)
                .await
                .context("select latest_ts_by_channel")?;
        let mut out = HashMap::with_capacity(rows.len());
        for r in rows {
            let cid: String = r.try_get("channel_id").unwrap_or_default();
            let ts: Option<String> = r.try_get("max_ts").ok();
            if let Some(ts) = ts {
                out.insert(cid, ts);
            }
        }
        Ok(out)
    }

    // ── replies_pages ───────────────────────────────────────────────

    /// Record the latest reply we have for a thread, so the next sync's
    /// `conversations.replies` walk can skip threads that haven't
    /// gained new replies since.
    pub async fn upsert_replies_page(
        &self,
        channel_id: &str,
        thread_ts: &str,
        latest_reply: Option<&str>,
    ) -> Result<()> {
        let id = format!("{channel_id}:{thread_ts}");
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO replies_pages
                (id, channel_id, thread_ts, latest_reply, fetched_at, last_attempt_at, last_error)
             VALUES (?, ?, ?, ?, ?, ?, NULL)
             ON CONFLICT(id) DO UPDATE SET
                latest_reply = COALESCE(excluded.latest_reply, replies_pages.latest_reply),
                fetched_at = excluded.fetched_at,
                last_attempt_at = excluded.last_attempt_at,
                last_error = NULL",
        )
        .bind(&id)
        .bind(channel_id)
        .bind(thread_ts)
        .bind(latest_reply)
        .bind(&now)
        .bind(&now)
        .execute(&self.pool)
        .await
        .with_context(|| format!("upsert replies_page {id}"))?;
        Ok(())
    }

    /// `(channel_id, thread_ts) → latest_reply` for every thread we've
    /// already walked. Used to skip redundant `conversations.replies`
    /// calls on the next sync.
    pub async fn latest_reply_by_thread(&self) -> Result<HashMap<(String, String), String>> {
        let rows = sqlx::query(
            "SELECT channel_id, thread_ts, latest_reply FROM replies_pages
             WHERE latest_reply IS NOT NULL",
        )
        .fetch_all(&self.pool)
        .await
        .context("select latest_reply_by_thread")?;
        let mut out = HashMap::with_capacity(rows.len());
        for r in rows {
            let cid: String = r.try_get("channel_id").unwrap_or_default();
            let tts: String = r.try_get("thread_ts").unwrap_or_default();
            let lr: String = r.try_get("latest_reply").unwrap_or_default();
            if !cid.is_empty() && !tts.is_empty() && !lr.is_empty() {
                out.insert((cid, tts), lr);
            }
        }
        Ok(out)
    }

    // ── blobs (delegates) ───────────────────────────────────────────

    pub async fn blob_exists(&self, id: &str) -> Result<bool> {
        dr::blob_exists(&self.pool, id).await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn upsert_blob_bytes(
        &self,
        id: &str,
        kind: &str,
        owning_id: &str,
        slot: &str,
        content_type: Option<&str>,
        bytes: &[u8],
        source_url: Option<&str>,
    ) -> Result<()> {
        dr::upsert_blob_bytes(
            &self.pool,
            id,
            kind,
            owning_id,
            slot,
            content_type,
            bytes,
            source_url,
        )
        .await
    }

    pub async fn record_blob_error(
        &self,
        id: &str,
        owning_id: &str,
        slot: &str,
        err: &str,
    ) -> Result<()> {
        dr::record_blob_error(&self.pool, id, owning_id, slot, err).await
    }

    pub async fn load_blobs_by_id(&self) -> Result<HashMap<String, BlobBytes>> {
        dr::load_blobs_by_id(&self.pool).await
    }
}

/// One row's worth of loaded message data — payload plus the columns
/// the translate path needs at hand. Mirrors [`MessageRow`] minus the
/// upstream JSON quirks we already promoted to columns.
#[derive(Debug, Clone)]
pub struct LoadedMessage {
    pub id: String,
    pub team_id: String,
    pub channel_id: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub is_thread_root: bool,
    pub user_id: Option<String>,
    pub payload: Value,
}

/// Bag returned to the synchronous translate path.
#[derive(Debug, Default, Clone)]
pub struct LoadedRaw {
    pub workspace: Option<Value>,
    pub users: Vec<Value>,
    pub channels: Vec<Value>,
    pub messages: Vec<LoadedMessage>,
    pub blobs_by_id: HashMap<String, BlobBytes>,
}

/// Synchronous helper for non-async callers (translate, synthesize)
/// that already run under `#[tokio::main]`. Uses `block_in_place` +
/// the current Handle, so it must be invoked on a multi-thread runtime.
pub fn block_on_load_all(db_path: &Path) -> Result<LoadedRaw> {
    let path = db_path.to_path_buf();
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let db = RawDb::open(&path).await?;
            Ok::<_, anyhow::Error>(LoadedRaw {
                workspace: db.load_workspace().await?,
                users: db.load_users().await?,
                channels: db.load_channels().await?,
                messages: db.load_messages().await?,
                blobs_by_id: db.load_blobs_by_id().await?,
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn workspace_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("s.doltlite_db")).await.unwrap();
        db.upsert_workspace(&json!({
            "team_id": "T1", "team": "Enterprise", "url": "https://e.slack.com/", "user_id": "U1"
        }))
        .await
        .unwrap();
        let w = db.load_workspace().await.unwrap().expect("workspace");
        assert_eq!(w["team_id"], "T1");
        assert_eq!(db.cached_team_id().await.unwrap().as_deref(), Some("T1"));
    }

    #[tokio::test]
    async fn message_round_trips_and_dedupes() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("s.doltlite_db")).await.unwrap();
        let row = MessageRow {
            team_id: "T1".into(),
            channel_id: "C1".into(),
            ts: "1700000000.000100".into(),
            thread_ts: None,
            is_thread_root: true,
            user_id: Some("U1".into()),
            payload: json!({"ts": "1700000000.000100", "text": "hi", "user": "U1"}),
        };
        db.upsert_message(&row).await.unwrap();
        // Re-insert: same ts collapses to one row.
        db.upsert_message(&row).await.unwrap();
        let msgs = db.load_messages().await.unwrap();
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].channel_id, "C1");
        assert_eq!(msgs[0].ts, "1700000000.000100");
    }

    #[tokio::test]
    async fn payload_stored_as_jsonb_blob() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("s.doltlite_db")).await.unwrap();
        db.upsert_channel(&json!({"id": "C1", "name": "general", "is_member": true}))
            .await
            .unwrap();
        let row = sqlx::query("SELECT typeof(payload) AS t FROM channels WHERE id='C1'")
            .fetch_one(db.pool())
            .await
            .unwrap();
        let t: String = row.try_get("t").unwrap();
        assert_eq!(t, "blob", "payload should be JSONB-encoded BLOB");
    }

    #[tokio::test]
    async fn channels_for_fetch_honors_filters() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("s.doltlite_db")).await.unwrap();
        db.upsert_channel(
            &json!({"id": "C1", "name": "a", "is_member": true, "is_archived": false}),
        )
        .await
        .unwrap();
        db.upsert_channel(
            &json!({"id": "C2", "name": "b", "is_member": false, "is_archived": false}),
        )
        .await
        .unwrap();
        db.upsert_channel(
            &json!({"id": "C3", "name": "c", "is_member": true, "is_archived": true}),
        )
        .await
        .unwrap();
        let mem_only = db.channels_for_fetch(true, false).await.unwrap();
        assert_eq!(
            mem_only.iter().map(|(i, _)| i.as_str()).collect::<Vec<_>>(),
            vec!["C1"]
        );
        let with_archived = db.channels_for_fetch(true, true).await.unwrap();
        assert_eq!(
            with_archived
                .iter()
                .map(|(i, _)| i.as_str())
                .collect::<Vec<_>>(),
            vec!["C1", "C3"]
        );
    }

    #[tokio::test]
    async fn latest_reply_by_thread_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("s.doltlite_db")).await.unwrap();
        db.upsert_replies_page("C1", "1.0", Some("2.0"))
            .await
            .unwrap();
        db.upsert_replies_page("C1", "3.0", Some("4.0"))
            .await
            .unwrap();
        let m = db.latest_reply_by_thread().await.unwrap();
        assert_eq!(
            m.get(&("C1".to_string(), "1.0".to_string()))
                .map(String::as_str),
            Some("2.0")
        );
        assert_eq!(
            m.get(&("C1".to_string(), "3.0".to_string()))
                .map(String::as_str),
            Some("4.0")
        );
    }
}
