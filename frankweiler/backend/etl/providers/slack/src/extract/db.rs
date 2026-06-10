//! Doltlite-backed raw store for the Slack provider.
//!
//! Replaces the JSONL tree of `raw_api/<method>/run-*.jsonl` with a
//! single sqlite file at `<data_root>/raw/<name>.doltlite_db`. Shared
//! bookkeeping tables (`blobs`, `sync_runs`) and the
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

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::{json, Value};
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use frankweiler_etl::blob_cas::{
    self, BlobCas, BlobReader, InMemoryBlobReader, RefStub, SqliteBlobReader,
};
use frankweiler_etl::doltlite_raw::{self as dr};
use frankweiler_etl::event_tape::EventTape;

pub use frankweiler_etl::doltlite_raw::db_path_for;

use super::schema_raw::{
    full_ddl, replies_page_id_recipe, slack_message_uuid, slack_thread_uuid, DATA_TABLES,
};

#[derive(Clone, Debug)]
pub struct RawDb {
    pool: SqlitePool,
    cas: BlobCas,
    /// Optional plain-text mirror of every upsert. See
    /// `docs/data_architecture.md` § "Wire-event tape (JSONL)".
    /// `None` = tape disabled (the default); cloned `RawDb`s share the
    /// same tape via `Arc`.
    tape: Option<Arc<EventTape>>,
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
        let owned = full_ddl();
        let slices: Vec<&str> = owned.iter().map(String::as_str).collect();
        let pool = dr::open(db_path, &slices).await?;
        // Backfill thread_root_uuid for rows written by older versions
        // that left it NULL on standalone messages. Idempotent: once
        // every row has a non-NULL thread_root_uuid, the SELECT below
        // returns no rows and the loop exits without writing.
        backfill_thread_root_uuid(&pool).await?;
        let cas = BlobCas::open(&blob_cas::cas_path_for(db_path)).await?;
        Ok(Self {
            pool,
            cas,
            tape: None,
        })
    }

    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    pub fn cas(&self) -> &BlobCas {
        &self.cas
    }

    /// Attach a JSONL event tape. Every upsert is mirrored as one line
    /// in `<tape.dir>/<table>.jsonl` in addition to landing in doltlite.
    /// The tape is shared by clones of this `RawDb`.
    pub fn attach_event_tape(&mut self, tape: Arc<EventTape>) {
        self.tape = Some(tape);
    }

    fn tape_ref(&self) -> Option<&EventTape> {
        self.tape.as_deref()
    }

    /// Wipe every per-row table so the next fetch re-downloads
    /// everything from upstream. See
    /// [`frankweiler_etl::doltlite_raw::truncate_data_tables`]. Also
    /// clears the slack-scoped manifest-sweep markers in
    /// `sync_scope_state` so the next run actually refetches the
    /// channel/user lists rather than honoring a stale TTL skip.
    pub async fn reset(&self) -> Result<()> {
        dr::truncate_data_tables(&self.pool, DATA_TABLES).await?;
        sqlx::query("DELETE FROM sync_scope_state WHERE scope LIKE 'slack:sweep:%'")
            .execute(&self.pool)
            .await
            .context("clear slack manifest sweep markers on reset")?;
        Ok(())
    }

    /// Age of the most recent successful sweep for `key` (e.g.
    /// `"channels:members_only=false:archived=true"`), or `None` if no
    /// sweep has ever completed. Backed by the shared
    /// `sync_scope_state` table; the scope string is namespaced as
    /// `slack:sweep:<key>` so `reset()` can wipe all slack entries with
    /// a single `LIKE`.
    pub async fn manifest_sweep_age(&self, key: &str) -> Result<Option<chrono::Duration>> {
        let scope = format!("slack:sweep:{key}");
        let row = sqlx::query("SELECT last_seen_at FROM sync_scope_state WHERE scope = ?")
            .bind(&scope)
            .fetch_optional(&self.pool)
            .await
            .context("select manifest sweep marker")?;
        let Some(row) = row else { return Ok(None) };
        let s: String = row
            .try_get("last_seen_at")
            .context("read manifest sweep timestamp")?;
        let dt = chrono::DateTime::parse_from_rfc3339(&s)
            .with_context(|| format!("parse manifest sweep timestamp {s:?}"))?
            .with_timezone(&Utc);
        Ok(Some(Utc::now() - dt))
    }

    /// Stamp `key`'s sweep as completed at `now()`. Call this only
    /// after every page of the sweep has been written, so an interrupted
    /// sweep doesn't poison the TTL check.
    pub async fn record_manifest_sweep(&self, key: &str) -> Result<()> {
        let scope = format!("slack:sweep:{key}");
        let now = Utc::now().to_rfc3339();
        sqlx::query(
            "INSERT INTO sync_scope_state (scope, last_seen_at) VALUES (?, ?) \
             ON CONFLICT(scope) DO UPDATE SET last_seen_at = excluded.last_seen_at",
        )
        .bind(&scope)
        .bind(&now)
        .execute(&self.pool)
        .await
        .context("record manifest sweep marker")?;
        Ok(())
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
        let mut tx = self.pool.begin().await.context("begin workspace tx")?;
        sqlx::query(
            "INSERT INTO workspaces
                (id, team_name, team_url, self_user_id, payload)
             VALUES (?, ?, ?, ?, jsonb(?))
             ON CONFLICT(id) DO UPDATE SET
                team_name = COALESCE(excluded.team_name, workspaces.team_name),
                team_url = COALESCE(excluded.team_url, workspaces.team_url),
                self_user_id = COALESCE(excluded.self_user_id, workspaces.self_user_id),
                payload = excluded.payload",
        )
        .bind(team_id)
        .bind(team_name)
        .bind(team_url)
        .bind(self_user_id)
        .bind(&payload_str)
        .execute(&mut *tx)
        .await
        .context("upsert workspace")?;
        dr::write_event_to_raw_storage_layer(tx, self.tape_ref(), "workspaces", team_id, payload)
            .await
    }

    /// Return the cached workspace `team_id` (the most-recently-seen
    /// row) so callers that need it before re-fetching `auth.test`
    /// don't have to walk the payload. `fetched_at` now lives on
    /// `workspaces_bookkeeping`; LEFT JOIN keeps the same recency
    /// ordering.
    pub async fn cached_team_id(&self) -> Result<Option<String>> {
        let row = sqlx::query(
            "SELECT w.id FROM workspaces w \
             LEFT JOIN workspaces_bookkeeping b ON b.id = w.id \
             ORDER BY b.fetched_at DESC LIMIT 1",
        )
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
        self.upsert_users(std::slice::from_ref(payload)).await
    }

    /// Upsert a whole `users.list` page in a single transaction. One
    /// `fsync` per page instead of per row makes Slack's listing phase
    /// ~100× cheaper on contended sqlite.
    pub async fn upsert_users(&self, payloads: &[Value]) -> Result<()> {
        if payloads.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.context("begin users batch tx")?;
        let now = Utc::now().to_rfc3339();
        for payload in payloads {
            upsert_user_in(&mut tx, payload, &now).await?;
        }
        let events: Vec<(&str, &str, &Value)> = payloads
            .iter()
            .filter_map(|p| {
                p.get("id")
                    .and_then(|v| v.as_str())
                    .map(|id| ("users", id, p))
            })
            .collect();
        dr::write_events_to_raw_storage_layer(tx, self.tape_ref(), &events).await
    }

    pub async fn load_users(&self) -> Result<Vec<Value>> {
        dr::load_payloads(&self.pool, "users").await
    }

    // ── channels ────────────────────────────────────────────────────

    pub async fn upsert_channel(&self, payload: &Value) -> Result<()> {
        self.upsert_channels(std::slice::from_ref(payload)).await
    }

    /// Upsert a whole `conversations.list` page in a single transaction.
    pub async fn upsert_channels(&self, payloads: &[Value]) -> Result<()> {
        if payloads.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.context("begin channels batch tx")?;
        let now = Utc::now().to_rfc3339();
        for payload in payloads {
            upsert_channel_in(&mut tx, payload, &now).await?;
        }
        let events: Vec<(&str, &str, &Value)> = payloads
            .iter()
            .filter_map(|p| {
                p.get("id")
                    .and_then(|v| v.as_str())
                    .map(|id| ("channels", id, p))
            })
            .collect();
        dr::write_events_to_raw_storage_layer(tx, self.tape_ref(), &events).await
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
        self.upsert_messages(std::slice::from_ref(row)).await
    }

    /// Upsert a whole history / replies page in a single transaction.
    pub async fn upsert_messages(&self, rows: &[MessageRow]) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut tx = self.pool.begin().await.context("begin messages batch tx")?;
        let now = Utc::now().to_rfc3339();
        for row in rows {
            upsert_message_in(&mut tx, row, &now).await?;
        }
        let ids: Vec<String> = rows
            .iter()
            .map(|r| slack_message_uuid(&r.team_id, &r.channel_id, &r.ts))
            .collect();
        let events: Vec<(&str, &str, &Value)> = ids
            .iter()
            .zip(rows.iter())
            .map(|(id, r)| ("messages", id.as_str(), &r.payload))
            .collect();
        dr::write_events_to_raw_storage_layer(tx, self.tape_ref(), &events).await
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
        let id = replies_page_id_recipe(channel_id, thread_ts);
        let mut tx = self.pool.begin().await.context("begin replies_page tx")?;
        sqlx::query(
            "INSERT INTO replies_pages
                (id, channel_id, thread_ts, latest_reply)
             VALUES (?, ?, ?, ?)
             ON CONFLICT(id) DO UPDATE SET
                latest_reply = COALESCE(excluded.latest_reply, replies_pages.latest_reply)",
        )
        .bind(&id)
        .bind(channel_id)
        .bind(thread_ts)
        .bind(latest_reply)
        .execute(&mut *tx)
        .await
        .with_context(|| format!("upsert replies_page {id}"))?;
        let payload = json!({
            "channel_id": channel_id,
            "thread_ts": thread_ts,
            "latest_reply": latest_reply,
        });
        dr::write_event_to_raw_storage_layer(tx, self.tape_ref(), "replies_pages", &id, &payload)
            .await
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

    // ── blobs (delegate to shared `blob_cas`) ───────────────────────

    pub async fn blob_exists(&self, ref_id: &str) -> Result<bool> {
        blob_cas::ref_has_hash(&self.pool, ref_id).await
    }

    /// Snapshot every ref_id that already has a hash attached. Loaded
    /// once at run start so the per-file `download_one_file` skip-check
    /// is a HashSet hit instead of a SQLite round trip.
    pub async fn loaded_blob_ids(&self) -> Result<HashSet<String>> {
        let rows = sqlx::query("SELECT id FROM blob_refs WHERE blake3 IS NOT NULL")
            .fetch_all(&self.pool)
            .await
            .context("load blob ids with bytes")?;
        let mut out = HashSet::with_capacity(rows.len());
        for r in rows {
            if let Ok(id) = r.try_get::<String, _>("id") {
                out.insert(id);
            }
        }
        Ok(out)
    }

    pub async fn pre_seed_blob_stub(
        &self,
        ref_id: &str,
        owning_id: &str,
        content_type: Option<&str>,
        source_url: Option<&str>,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await.context("begin blob stub tx")?;
        blob_cas::pre_seed_ref(
            &mut tx,
            &RefStub {
                ref_id,
                kind: "file",
                owning_id,
                slot: "file",
                upstream_uuid: Some(ref_id),
                upstream_name: None,
                source_url,
                content_type,
            },
        )
        .await?;
        tx.commit().await.context("commit blob stub tx")?;
        Ok(())
    }

    pub async fn store_blob(&self, stub: &RefStub<'_>, bytes: &[u8]) -> Result<String> {
        blob_cas::store_bytes(&self.pool, &self.cas, stub, bytes).await
    }

    pub async fn record_blob_error(
        &self,
        ref_id: &str,
        owning_id: &str,
        slot: &str,
        err: &str,
    ) -> Result<()> {
        let mut tx = self.pool.begin().await.context("begin blob error tx")?;
        blob_cas::record_ref_error(&mut tx, ref_id, owning_id, slot, err).await?;
        tx.commit().await.context("commit blob error tx")?;
        Ok(())
    }
}

// ── private row-level upserts (shared by single + batch APIs) ──────────

async fn upsert_user_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    payload: &Value,
    _now: &str,
) -> Result<()> {
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
    sqlx::query(
        "INSERT INTO users
            (id, team_id, name, real_name, display_name, payload)
         VALUES (?, ?, ?, ?, ?, jsonb(?))
         ON CONFLICT(id) DO UPDATE SET
            team_id = COALESCE(excluded.team_id, users.team_id),
            name = COALESCE(excluded.name, users.name),
            real_name = COALESCE(excluded.real_name, users.real_name),
            display_name = COALESCE(excluded.display_name, users.display_name),
            payload = excluded.payload",
    )
    .bind(id)
    .bind(team_id)
    .bind(name)
    .bind(real_name)
    .bind(display_name)
    .bind(&payload_str)
    .execute(&mut **tx)
    .await
    .with_context(|| format!("upsert user {id}"))?;
    Ok(())
}

async fn upsert_channel_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    payload: &Value,
    _now: &str,
) -> Result<()> {
    let id = payload
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("channel response missing id"))?;
    let name = payload.get("name").and_then(|v| v.as_str());
    let is_member = payload.get("is_member").and_then(|v| v.as_bool());
    let is_archived = payload.get("is_archived").and_then(|v| v.as_bool());
    let payload_str = serde_json::to_string(payload).context("serialize channel")?;
    sqlx::query(
        "INSERT INTO channels
            (id, name, is_member, is_archived, payload)
         VALUES (?, ?, ?, ?, jsonb(?))
         ON CONFLICT(id) DO UPDATE SET
            name = COALESCE(excluded.name, channels.name),
            is_member = COALESCE(excluded.is_member, channels.is_member),
            is_archived = COALESCE(excluded.is_archived, channels.is_archived),
            payload = excluded.payload",
    )
    .bind(id)
    .bind(name)
    .bind(is_member.map(|b| b as i64))
    .bind(is_archived.map(|b| b as i64))
    .bind(&payload_str)
    .execute(&mut **tx)
    .await
    .with_context(|| format!("upsert channel {id}"))?;
    Ok(())
}

async fn upsert_message_in(
    tx: &mut sqlx::Transaction<'_, sqlx::Sqlite>,
    row: &MessageRow,
    _now: &str,
) -> Result<()> {
    let id = slack_message_uuid(&row.team_id, &row.channel_id, &row.ts);
    // Every message belongs to some thread — either a real reply thread
    // (thread_ts present) or a standalone "thread of one" whose
    // effective_thread_ts is the message's own ts. Stamping
    // thread_root_uuid for both keeps `messages_by_thread` covering every
    // row, so the translate-side cheap probe (`GROUP BY
    // thread_root_uuid`) and per-thread filtered load
    // (`WHERE thread_root_uuid IN (...)`) hit the index instead of
    // scanning + sorting.
    let effective_thread_ts = row.thread_ts.as_deref().unwrap_or(row.ts.as_str());
    let thread_root_uuid = Some(slack_thread_uuid(
        &row.team_id,
        &row.channel_id,
        effective_thread_ts,
    ));
    let payload_str = serde_json::to_string(&row.payload).context("serialize message")?;
    sqlx::query(
        "INSERT INTO messages
            (id, team_id, channel_id, ts, thread_ts, thread_root_uuid, is_thread_root,
             user_id, payload)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, jsonb(?))
         ON CONFLICT(id) DO UPDATE SET
            team_id = excluded.team_id,
            channel_id = excluded.channel_id,
            ts = excluded.ts,
            thread_ts = COALESCE(excluded.thread_ts, messages.thread_ts),
            thread_root_uuid = COALESCE(excluded.thread_root_uuid, messages.thread_root_uuid),
            is_thread_root = COALESCE(excluded.is_thread_root, messages.is_thread_root),
            user_id = COALESCE(excluded.user_id, messages.user_id),
            payload = excluded.payload",
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
    .execute(&mut **tx)
    .await
    .with_context(|| format!("upsert message {id}"))?;
    Ok(())
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

/// Bag returned to the synchronous translate path. `blobs` is a
/// streaming handle (not a bulk-loaded map): render fetches one blob's
/// bytes at a time, so peak RSS stays low even for sources with
/// hundreds of multi-MB attachments.
#[derive(Clone)]
pub struct LoadedRaw {
    pub workspace: Option<Value>,
    pub users: Vec<Value>,
    pub channels: Vec<Value>,
    pub messages: Vec<LoadedMessage>,
    pub blobs: std::sync::Arc<dyn BlobReader>,
}

impl Default for LoadedRaw {
    fn default() -> Self {
        Self {
            workspace: None,
            users: Vec::new(),
            channels: Vec::new(),
            messages: Vec::new(),
            blobs: InMemoryBlobReader::empty_handle(),
        }
    }
}

/// Synchronous helper for non-async callers (translate, synthesize)
/// that already run under `#[tokio::main]`. Uses `block_in_place` +
/// the current Handle, so it must be invoked on a multi-thread runtime.
/// Stamp `thread_root_uuid` for any pre-existing rows where it's NULL.
/// Older versions only set it when `thread_ts` was non-null, leaving
/// standalone messages outside the `messages_by_thread` index. After
/// this runs once, every message is covered. Paged so memory stays
/// bounded on large DBs.
async fn backfill_thread_root_uuid(pool: &SqlitePool) -> Result<()> {
    const PAGE: i64 = 10_000;
    loop {
        let rows = sqlx::query(
            "SELECT id, team_id, channel_id, ts FROM messages
             WHERE thread_root_uuid IS NULL LIMIT ?",
        )
        .bind(PAGE)
        .fetch_all(pool)
        .await
        .context("scan messages with NULL thread_root_uuid")?;
        if rows.is_empty() {
            return Ok(());
        }
        let mut tx = pool.begin().await.context("begin backfill tx")?;
        for r in &rows {
            let id: String = r.try_get("id").context("backfill id")?;
            let team_id: String = r.try_get("team_id").context("backfill team_id")?;
            let channel_id: String = r.try_get("channel_id").context("backfill channel_id")?;
            let ts: String = r.try_get("ts").context("backfill ts")?;
            let uuid = slack_thread_uuid(&team_id, &channel_id, &ts);
            sqlx::query("UPDATE messages SET thread_root_uuid = ? WHERE id = ?")
                .bind(&uuid)
                .bind(&id)
                .execute(&mut *tx)
                .await
                .with_context(|| format!("backfill update {id}"))?;
        }
        tx.commit().await.context("commit backfill tx")?;
    }
}

/// Cheap probe: `(thread_root_uuid → "<MAX(fetched_at)>|<COUNT(*)>")`
/// for every thread in the DB, grouped via the `messages_by_thread`
/// index. The orchestrator compares each entry against the cursor it
/// stored on the prior render — threads whose cursor matches don't get
/// their payloads loaded at all. Both fields move on any upsert (slack
/// extract bumps `fetched_at` unconditionally) so the cursor is a
/// conservative "did anything change in this thread" signal.
pub async fn probe_thread_cursors(db_path: &Path) -> Result<HashMap<String, String>> {
    let db = RawDb::open(db_path).await?;
    // `fetched_at` now lives on the bookkeeping sidecar — LEFT JOIN
    // by message id and aggregate as before. Semantics preserved:
    // any upsert into a thread (including no-op re-fetch) bumps the
    // sidecar's fetched_at, so MAX(fetched_at) still moves on every
    // run — the conservative "did anything change in this thread"
    // signal the orchestrator wants.
    let rows = sqlx::query(
        "SELECT m.thread_root_uuid,
                MAX(b.fetched_at) AS max_fetched_at,
                COUNT(*) AS msg_count
         FROM messages m
         LEFT JOIN messages_bookkeeping b ON b.id = m.id
         WHERE m.payload IS NOT NULL AND m.thread_root_uuid IS NOT NULL
         GROUP BY m.thread_root_uuid",
    )
    .fetch_all(&db.pool)
    .await
    .context("probe_thread_cursors")?;
    let mut out: HashMap<String, String> = HashMap::with_capacity(rows.len());
    for r in rows {
        let uuid: String = r.try_get("thread_root_uuid").unwrap_or_default();
        if uuid.is_empty() {
            continue;
        }
        let max_ts: String = r.try_get("max_fetched_at").unwrap_or_default();
        let count: i64 = r.try_get("msg_count").unwrap_or(0);
        out.insert(uuid, format!("{max_ts}|{count}"));
    }
    Ok(out)
}

/// Synchronous wrapper around [`probe_thread_cursors`] for translate /
/// orchestrator callers that already sit under `#[tokio::main]`.
pub fn block_on_probe_thread_cursors(db_path: &Path) -> Result<HashMap<String, String>> {
    let path = db_path.to_path_buf();
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move { probe_thread_cursors(&path).await })
    })
}

/// Filtered load: only messages whose `thread_root_uuid` is in the
/// provided set. Used by the cursor-aware translate path to skip
/// loading payloads for threads that the cheap probe says haven't
/// changed since the last render. Ordering is `thread_root_uuid, ts`
/// so downstream consumers see each thread's messages contiguously.
/// Param batching keeps each query under SQLite's bind limit.
pub fn block_on_load_filtered(
    db_path: &Path,
    thread_uuids: &std::collections::HashSet<String>,
) -> Result<LoadedRaw> {
    let path = db_path.to_path_buf();
    let uuids: Vec<String> = thread_uuids.iter().cloned().collect();
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let db = RawDb::open(&path).await?;
            let workspace = db.load_workspace().await?;
            let users = db.load_users().await?;
            let channels = db.load_channels().await?;
            let messages = load_messages_for_threads(&db.pool, &uuids).await?;
            let blobs: std::sync::Arc<dyn BlobReader> = std::sync::Arc::new(SqliteBlobReader::new(
                db.pool().clone(),
                db.cas().pool().clone(),
            ));
            Ok::<_, anyhow::Error>(LoadedRaw {
                workspace,
                users,
                channels,
                messages,
                blobs,
            })
        })
    })
}

async fn load_messages_for_threads(
    pool: &SqlitePool,
    thread_uuids: &[String],
) -> Result<Vec<LoadedMessage>> {
    if thread_uuids.is_empty() {
        return Ok(Vec::new());
    }
    // SQLite's default SQLITE_LIMIT_VARIABLE_NUMBER is 32766 on modern
    // builds; stay well below it so we don't trip the limit on a
    // re-tuned build. 500 also keeps each query's plan compact.
    const CHUNK: usize = 500;
    let mut out: Vec<LoadedMessage> = Vec::new();
    for chunk in thread_uuids.chunks(CHUNK) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT id, team_id, channel_id, ts, thread_ts, is_thread_root, user_id,
                    json(payload) AS payload
             FROM messages
             WHERE payload IS NOT NULL AND thread_root_uuid IN ({placeholders})
             ORDER BY thread_root_uuid, ts"
        );
        let mut q = sqlx::query(&sql);
        for u in chunk {
            q = q.bind(u);
        }
        let rows = q
            .fetch_all(pool)
            .await
            .context("select messages for threads")?;
        out.reserve(rows.len());
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
    }
    Ok(out)
}

pub fn block_on_load_all(db_path: &Path) -> Result<LoadedRaw> {
    let path = db_path.to_path_buf();
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async move {
            let db = RawDb::open(&path).await?;
            let blobs: std::sync::Arc<dyn BlobReader> = std::sync::Arc::new(SqliteBlobReader::new(
                db.pool().clone(),
                db.cas().pool().clone(),
            ));
            Ok::<_, anyhow::Error>(LoadedRaw {
                workspace: db.load_workspace().await?,
                users: db.load_users().await?,
                channels: db.load_channels().await?,
                messages: db.load_messages().await?,
                blobs,
            })
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn event_tape_mirrors_upserts_to_jsonl() {
        let d = tempfile::tempdir().unwrap();
        let tape_dir = d.path().join("events");
        let tape = std::sync::Arc::new(EventTape::new(tape_dir.clone()));
        let mut db = RawDb::open(&d.path().join("s.doltlite_db")).await.unwrap();
        db.attach_event_tape(tape);

        db.upsert_workspace(&json!({"team_id": "T1", "team": "Enterprise"}))
            .await
            .unwrap();
        db.upsert_users(&[
            json!({"id": "U1", "name": "picard"}),
            json!({"id": "U2", "name": "riker"}),
        ])
        .await
        .unwrap();
        db.upsert_channel(&json!({"id": "C1", "name": "bridge"}))
            .await
            .unwrap();
        db.upsert_message(&MessageRow {
            team_id: "T1".into(),
            channel_id: "C1".into(),
            ts: "1700000000.000100".into(),
            thread_ts: None,
            is_thread_root: true,
            user_id: Some("U1".into()),
            payload: json!({"ts": "1700000000.000100", "text": "make it so"}),
        })
        .await
        .unwrap();

        let workspaces = std::fs::read_to_string(tape_dir.join("workspaces.jsonl")).unwrap();
        assert_eq!(workspaces.lines().count(), 1);
        let line: Value = serde_json::from_str(workspaces.lines().next().unwrap()).unwrap();
        assert_eq!(line["table"], "workspaces");
        assert_eq!(line["id"], "T1");
        assert_eq!(line["payload"]["team"], "Enterprise");

        let users = std::fs::read_to_string(tape_dir.join("users.jsonl")).unwrap();
        assert_eq!(users.lines().count(), 2);

        let channels = std::fs::read_to_string(tape_dir.join("channels.jsonl")).unwrap();
        assert_eq!(channels.lines().count(), 1);

        let messages = std::fs::read_to_string(tape_dir.join("messages.jsonl")).unwrap();
        assert_eq!(messages.lines().count(), 1);
        let m: Value = serde_json::from_str(messages.lines().next().unwrap()).unwrap();
        assert_eq!(m["payload"]["text"], "make it so");
    }

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
