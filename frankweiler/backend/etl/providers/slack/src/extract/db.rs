//! Doltlite-backed raw store for the Slack provider.
//!
//! Six tables — `workspaces`, `users`, `channels`, `messages`,
//! `replies_pages`, `slack_attachments` — shared bookkeeping
//! (`<table>_bookkeeping`, `sync_runs`, …) lives in
//! [`frankweiler_etl::doltlite_raw`]. Per the dolt_diff + per-provider
//! CAS edge migration: attachment bytes ride in the shared
//! `cas_objects`, but the (file_id → blake3) mapping lives on
//! `slack_attachments` rather than the shared `blob_refs`.
//!
//! No listing pre-seed: rows only appear after a successful detail
//! fetch. See `schema_raw.rs` for the rationale.
//!
//! ## Wire-event tape
//!
//! `RawDb::attach_event_tape` wires a JSONL mirror of every entity
//! upsert. The tape append fires after the doltlite commit succeeds;
//! see [`docs/data_architecture_ingestion.md`] § "Wire-event tape
//! (JSONL)".

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::Value;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use frankweiler_etl::blob_cas::BlobCas;
use frankweiler_etl::bulk::bulk_upsert_in_tx;
use frankweiler_etl::doltlite_raw::{self as dr, bulk_upsert_with_tape};
use frankweiler_etl::event_tape::EventTape;

pub use frankweiler_etl::doltlite_raw::db_path_for;

use super::schema_raw::{
    full_ddl, slack_message_uuid, slack_thread_uuid, ChannelRow, MessageRow, UserRow, WorkspaceRow,
    DATA_TABLES,
};
use frankweiler_etl::doltlite_raw::WirePayload;

#[derive(Clone, Debug)]
pub struct RawDb {
    pool: SqlitePool,
    cas: BlobCas,
    /// Optional plain-text mirror of every upsert. `None` = tape
    /// disabled (the default); cloned `RawDb`s share the same tape
    /// via `Arc`.
    tape: Option<Arc<EventTape>>,
}

impl RawDb {
    pub async fn open(db_path: &Path) -> Result<Self> {
        let owned = full_ddl();
        let slices: Vec<&str> = owned.iter().map(String::as_str).collect();
        let pool = dr::open(db_path, &slices).await?;
        let cas = BlobCas::open(&frankweiler_etl::blob_cas::cas_path_for(db_path)).await?;
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

    /// Attach a JSONL event tape. Every entity upsert is mirrored as
    /// one line in `<tape.dir>/<table>.jsonl` in addition to landing
    /// in doltlite. The tape is shared by clones of this `RawDb`.
    pub fn attach_event_tape(&mut self, tape: Arc<EventTape>) {
        self.tape = Some(tape);
    }

    fn tape_ref(&self) -> Option<&EventTape> {
        self.tape.as_deref()
    }

    /// Wipe every per-row table so the next fetch re-downloads
    /// everything. Also clears the slack-scoped manifest-sweep markers
    /// in `sync_scope_state` so a stale TTL doesn't suppress the
    /// channel/user refetch.
    pub async fn reset(&self) -> Result<()> {
        dr::truncate_data_tables(&self.pool, DATA_TABLES).await?;
        sqlx::query("DELETE FROM sync_scope_state WHERE scope LIKE 'slack:sweep:%'")
            .execute(&self.pool)
            .await
            .context("clear slack manifest sweep markers on reset")?;
        Ok(())
    }

    /// Replaces `truncate_blob_refs` for this provider: clear the
    /// per-provider `blake3` column so the next walk re-decodes and
    /// re-stores.
    pub async fn clear_blob_hashes(&self) -> Result<()> {
        sqlx::query("UPDATE slack_attachments SET blake3 = NULL")
            .execute(&self.pool)
            .await
            .context("clear slack_attachments.blake3")?;
        Ok(())
    }

    /// Age of the most recent successful sweep for `key`.
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
        let dt = frankweiler_time::parse_strict(&s)
            .with_context(|| format!("parse manifest sweep timestamp {s:?}"))?
            .inner()
            .with_timezone(&Utc);
        Ok(Some(Utc::now() - dt))
    }

    /// Stamp `key`'s sweep as completed at `now()`. Call after every
    /// page of the sweep has been written so an interrupted sweep
    /// doesn't poison the TTL check.
    pub async fn record_manifest_sweep(&self, key: &str) -> Result<()> {
        let scope = format!("slack:sweep:{key}");
        let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
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
        let row = WorkspaceRow {
            id_and_payload: WirePayload {
                id: team_id.to_string(),
                payload: serde_json::to_string(payload).context("serialize auth.test")?,
            },
            team_name: payload
                .get("team")
                .and_then(|v| v.as_str())
                .map(String::from),
            team_url: payload
                .get("url")
                .and_then(|v| v.as_str())
                .map(String::from),
            self_user_id: payload
                .get("user_id")
                .and_then(|v| v.as_str())
                .map(String::from),
        };
        let payloads: Vec<(&str, &Value)> = vec![(team_id, payload)];
        bulk_upsert_with_tape(&self.pool, self.tape_ref(), &[row], &payloads).await
    }

    /// Return the cached workspace `team_id` so callers that need it
    /// before re-fetching `auth.test` don't have to walk the payload.
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

    pub async fn upsert_users(&self, payloads: &[Value]) -> Result<()> {
        if payloads.is_empty() {
            return Ok(());
        }
        let mut rows: Vec<UserRow> = Vec::with_capacity(payloads.len());
        let mut tape_pairs: Vec<(&str, &Value)> = Vec::with_capacity(payloads.len());
        for payload in payloads {
            let Some(id) = payload.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            let profile = payload.get("profile");
            let real_name = payload
                .get("real_name")
                .and_then(|v| v.as_str())
                .or_else(|| {
                    profile
                        .and_then(|p| p.get("real_name"))
                        .and_then(|v| v.as_str())
                });
            let display_name = profile
                .and_then(|p| p.get("display_name"))
                .and_then(|v| v.as_str());
            rows.push(UserRow {
                id_and_payload: WirePayload {
                    id: id.to_string(),
                    payload: serde_json::to_string(payload).context("serialize user")?,
                },
                team_id: payload
                    .get("team_id")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                name: payload
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                real_name: real_name.map(String::from),
                display_name: display_name.map(String::from),
            });
            tape_pairs.push((id, payload));
        }
        bulk_upsert_with_tape(&self.pool, self.tape_ref(), &rows, &tape_pairs).await
    }

    pub async fn load_users(&self) -> Result<Vec<Value>> {
        dr::load_payloads(&self.pool, "users").await
    }

    // ── channels ────────────────────────────────────────────────────

    pub async fn upsert_channel(&self, payload: &Value) -> Result<()> {
        self.upsert_channels(std::slice::from_ref(payload)).await
    }

    pub async fn upsert_channels(&self, payloads: &[Value]) -> Result<()> {
        if payloads.is_empty() {
            return Ok(());
        }
        let mut rows: Vec<ChannelRow> = Vec::with_capacity(payloads.len());
        let mut tape_pairs: Vec<(&str, &Value)> = Vec::with_capacity(payloads.len());
        for payload in payloads {
            let Some(id) = payload.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            rows.push(ChannelRow {
                id_and_payload: WirePayload {
                    id: id.to_string(),
                    payload: serde_json::to_string(payload).context("serialize channel")?,
                },
                name: payload
                    .get("name")
                    .and_then(|v| v.as_str())
                    .map(String::from),
                is_member: payload
                    .get("is_member")
                    .and_then(|v| v.as_bool())
                    .map(|b| b as i64),
                is_archived: payload
                    .get("is_archived")
                    .and_then(|v| v.as_bool())
                    .map(|b| b as i64),
            });
            tape_pairs.push((id, payload));
        }
        bulk_upsert_with_tape(&self.pool, self.tape_ref(), &rows, &tape_pairs).await
    }

    pub async fn load_channels(&self) -> Result<Vec<Value>> {
        dr::load_payloads(&self.pool, "channels").await
    }

    /// Channels we should iterate during a fetch run.
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

    /// Input shape used by extract callers. Mirrors the upstream raw
    /// message plus the typed-column extracts the writer already has
    /// at hand; the synthesized PK and `thread_root_uuid` are computed
    /// in [`upsert_messages`].
    pub async fn upsert_messages(&self, inputs: &[MessageInput]) -> Result<()> {
        if inputs.is_empty() {
            return Ok(());
        }
        // Pre-serialize payloads and synthesize PKs/thread_root_uuids
        // once so the tape mirror and the doltlite write see identical
        // shapes.
        struct Prepared<'a> {
            row: MessageRow,
            payload: &'a Value,
        }
        let mut prepared: Vec<Prepared> = Vec::with_capacity(inputs.len());
        for m in inputs {
            let id = slack_message_uuid(&m.team_id, &m.channel_id, &m.ts);
            let effective_thread_ts = m.thread_ts.as_deref().unwrap_or(m.ts.as_str());
            let thread_root_uuid =
                slack_thread_uuid(&m.team_id, &m.channel_id, effective_thread_ts);
            let payload_str = serde_json::to_string(&m.payload).context("serialize message")?;
            prepared.push(Prepared {
                row: MessageRow {
                    id_and_payload: WirePayload {
                        id,
                        payload: payload_str,
                    },
                    team_id: m.team_id.clone(),
                    channel_id: m.channel_id.clone(),
                    ts: m.ts.clone(),
                    thread_ts: m.thread_ts.clone(),
                    thread_root_uuid,
                    is_thread_root: m.is_thread_root as i64,
                    user_id: m.user_id.clone(),
                },
                payload: &m.payload,
            });
        }
        let rows: Vec<MessageRow> = prepared.iter().map(|p| p.row.clone()).collect();
        let tape_pairs: Vec<(&str, &Value)> = prepared
            .iter()
            .map(|p| (p.row.id_and_payload.id.as_str(), p.payload))
            .collect();
        bulk_upsert_with_tape(&self.pool, self.tape_ref(), &rows, &tape_pairs).await
    }

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
    /// cursor.
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

    pub async fn upsert_replies_page(
        &self,
        channel_id: &str,
        thread_ts: &str,
        latest_reply: Option<&str>,
    ) -> Result<()> {
        use super::schema_raw::{replies_page_id_recipe, RepliesPagesRow};
        let row = RepliesPagesRow {
            id: replies_page_id_recipe(channel_id, thread_ts),
            channel_id: channel_id.to_string(),
            thread_ts: thread_ts.to_string(),
            latest_reply: latest_reply.map(String::from),
        };
        let now = frankweiler_time::IsoOffsetTimestamp::now_local().to_rfc3339();
        let mut tx = self.pool.begin().await.context("begin replies_page tx")?;
        bulk_upsert_in_tx(&mut tx, std::slice::from_ref(&row), &now).await?;
        tx.commit().await.context("commit replies_page tx")?;
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

    // ── attachments (per-provider CAS edge) ─────────────────────────

    /// Snapshot `(file_id → blake3)` for every attachment whose bytes
    /// have ever landed in the CAS. Called once at the start of a
    /// fetch run so the per-file "have we got these bytes yet?"
    /// check is a HashMap hit instead of a SQLite round trip queued
    /// behind preceding multi-MB CAS commits on the single-connection
    /// doltlite pool.
    pub async fn load_attachment_blake3s(&self) -> Result<HashMap<String, String>> {
        frankweiler_etl::blob_cas::load_blake3_index(&self.pool, "slack_attachments", "file_id")
            .await
    }
}

/// One row of input for [`RawDb::upsert_messages`]. Carries the
/// upstream JSON body plus the columns we promote from it.
#[derive(Debug, Clone)]
pub struct MessageInput {
    pub team_id: String,
    pub channel_id: String,
    pub ts: String,
    pub thread_ts: Option<String>,
    pub is_thread_root: bool,
    pub user_id: Option<String>,
    /// Raw Slack message JSON, byte-for-byte.
    pub payload: Value,
}

/// One row's worth of loaded message data — payload plus the columns
/// the translate path needs at hand.
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

/// Bag returned to the synchronous translate path. No `BlobReader`
/// here — render attaches per-thread [`BlobBundle`]s separately via
/// `parse_doltlite_async`.
#[derive(Clone, Default)]
pub struct LoadedRaw {
    pub workspace: Option<Value>,
    pub users: Vec<Value>,
    pub channels: Vec<Value>,
    pub messages: Vec<LoadedMessage>,
}

/// Synchronous helper for tests + non-async callers that want a
/// snapshot of every entity table at a fixed point in time.
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
        db.upsert_messages(&[MessageInput {
            team_id: "T1".into(),
            channel_id: "C1".into(),
            ts: "1700000000.000100".into(),
            thread_ts: None,
            is_thread_root: true,
            user_id: Some("U1".into()),
            payload: json!({"ts": "1700000000.000100", "text": "make it so"}),
        }])
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
        let row = MessageInput {
            team_id: "T1".into(),
            channel_id: "C1".into(),
            ts: "1700000000.000100".into(),
            thread_ts: None,
            is_thread_root: true,
            user_id: Some("U1".into()),
            payload: json!({"ts": "1700000000.000100", "text": "hi", "user": "U1"}),
        };
        db.upsert_messages(std::slice::from_ref(&row))
            .await
            .unwrap();
        db.upsert_messages(std::slice::from_ref(&row))
            .await
            .unwrap();
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
