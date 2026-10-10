//! Doltlite-backed raw store for the Slack provider.

use datalib_etl::entity_store::CasEntityStore;
use datalib_etl_macros::RawStoreHandle;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sqlx::{Row, Sqlite, Transaction};

use datalib_etl::blob_cas::CasEdgeRow as _;
use datalib_etl::bulk::{bulk_upsert_in_tx, BulkUpsertable, EventBatch};
use datalib_etl::doltlite_raw::{self as dr, bulk_upsert_with_tape, bulk_upsert_with_tape_split};
use datalib_etl::event_tape::EventTape;
use datalib_etl_web::coverage::{self, Span};
use datalib_etl_web::owed::{Fetched, Listed, Outcome};

pub use datalib_etl::doltlite_raw::db_path_for;

use super::schema_raw::{
    full_ddl, join_dm_user_ids, parse_dm_user_ids, saved_item_key, slack_message_key,
    slack_thread_key, BookmarkRow, ChannelReadStateRow, ChannelRow, MessageRow, SavedItemRow,
    SlackAttachmentRow, UserRow, WorkspaceRow, CHANNEL_VOLATILE_PATHS, LADDER,
    MESSAGE_VOLATILE_PATHS, READ_STATE_VOLATILE_PATHS, THREADS, USER_VOLATILE_PATHS,
};
use datalib_etl::doltlite_raw::WirePayload;

#[derive(Clone, Debug, RawStoreHandle)]
pub struct RawDb {
    store: CasEntityStore,
    /// Optional plain-text mirror of every upsert. `None` = tape
    /// disabled (the default); cloned `RawDb`s share the same tape
    /// via `Arc`.
    tape: Option<Arc<EventTape>>,
}

impl std::ops::Deref for RawDb {
    type Target = CasEntityStore;

    fn deref(&self) -> &CasEntityStore {
        &self.store
    }
}

impl RawDb {
    pub async fn open(db_path: &Path) -> Result<Self> {
        Ok(Self {
            store: CasEntityStore::open_migrating(db_path, &full_ddl(), LADDER).await?,
            tape: None,
        })
    }

    pub fn attach_event_tape(&mut self, tape: Arc<EventTape>) {
        self.tape = Some(tape);
    }

    fn tape_ref(&self) -> Option<&EventTape> {
        self.tape.as_deref()
    }

    /// How long before `now`, the run's own, `key` was last listed whole.
    pub async fn manifest_sweep_age(
        &self,
        key: &str,
        now: &DateTime<Utc>,
    ) -> Result<Option<chrono::Duration>> {
        let scope = format!("slack:sweep:{key}");
        let row = sqlx::query("SELECT last_seen_at_utc FROM sync_scope_state WHERE scope = ?")
            .bind(&scope)
            .fetch_optional(self.pool())
            .await
            .context("select manifest sweep marker")?;
        let Some(row) = row else { return Ok(None) };
        let s: String = row
            .try_get("last_seen_at_utc")
            .context("read manifest sweep timestamp")?;
        let dt = datalib_time::parse_strict(&s)
            .with_context(|| format!("parse manifest sweep timestamp {s:?}"))?
            .inner()
            .with_timezone(&Utc);
        Ok(Some(*now - dt))
    }

    pub async fn record_manifest_sweep(&self, key: &str, now: &DateTime<Utc>) -> Result<()> {
        let scope = format!("slack:sweep:{key}");
        dr::upsert_scope_state(self.pool(), &scope, &now.to_rfc3339())
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
        bulk_upsert_with_tape(self.pool(), self.tape_ref(), &[row], &payloads).await
    }

    pub async fn cached_team_id(&self) -> Result<Option<String>> {
        let row = sqlx::query(
            "SELECT w.id FROM workspaces w \
             LEFT JOIN workspaces_bookkeeping b ON b.id = w.id \
             ORDER BY b.fetched_at_utc DESC LIMIT 1",
        )
        .fetch_optional(self.pool())
        .await
        .context("select cached team_id")?;
        Ok(row.and_then(|r| r.try_get::<String, _>("id").ok()))
    }

    pub async fn load_workspace(&self) -> Result<Option<Value>> {
        let row =
            sqlx::query("SELECT json(payload) AS payload FROM workspaces ORDER BY id LIMIT 1")
                .fetch_optional(self.pool())
                .await
                .context("select workspace")?;
        let Some(row) = row else { return Ok(None) };
        let payload: Option<String> = row.try_get("payload").ok();
        Ok(payload.and_then(|s| serde_json::from_str(&s).ok()))
    }

    // ── users ───────────────────────────────────────────────────────

    pub async fn upsert_users(&self, payloads: &[Value]) -> Result<()> {
        if payloads.is_empty() {
            return Ok(());
        }
        let mut rows: Vec<UserRow> = Vec::with_capacity(payloads.len());
        let mut tape_pairs: Vec<(&str, &Value)> = Vec::with_capacity(payloads.len());
        // Owns split-out volatile Values; see `upsert_channels`.
        let mut volatile_store: Vec<(&str, Value)> = Vec::with_capacity(payloads.len());
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
            // Split the per-fetch `updated` bookkeeping into the sidecar.
            let (base, volatile) = dr::split_volatile(payload, USER_VOLATILE_PATHS);
            rows.push(UserRow {
                id_and_payload: WirePayload {
                    id: id.to_string(),
                    payload: serde_json::to_string(&base).context("serialize user")?,
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
            if let Some(v) = volatile {
                volatile_store.push((id, v));
            }
        }
        let volatile_pairs: Vec<(&str, &Value)> =
            volatile_store.iter().map(|(id, v)| (*id, v)).collect();
        bulk_upsert_with_tape_split(
            self.pool(),
            self.tape_ref(),
            &rows,
            &tape_pairs,
            &volatile_pairs,
        )
        .await
    }

    pub async fn load_users(&self) -> Result<Vec<Value>> {
        dr::load_payloads(self.pool(), "users").await
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
        // Owns the split-out volatile Values so we can lend `&Value` to
        // the chokepoint below. Only channels that had volatile fields
        // land here.
        let mut volatile_store: Vec<(&str, Value)> = Vec::with_capacity(payloads.len());
        for payload in payloads {
            let Some(id) = payload.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            // Split the per-fetch `updated` bookkeeping out of the
            // content payload; the entity row stores `base`, the sidecar
            // stores `volatile`, and the tape still sees the full
            // original `payload`.
            let (base, volatile) = dr::split_volatile(payload, CHANNEL_VOLATILE_PATHS);
            let flag = |k: &str| payload.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
            let is_im = flag("is_im");
            let is_dm = is_im || flag("is_mpim");
            rows.push(ChannelRow {
                id_and_payload: WirePayload {
                    id: id.to_string(),
                    payload: serde_json::to_string(&base).context("serialize channel")?,
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
                is_dm: Some(is_dm as i64),
                dm_user_ids: is_dm
                    .then(|| join_dm_user_ids(&dm_participants(payload, is_im)))
                    .flatten(),
            });
            tape_pairs.push((id, payload));
            if let Some(v) = volatile {
                volatile_store.push((id, v));
            }
        }
        let volatile_pairs: Vec<(&str, &Value)> =
            volatile_store.iter().map(|(id, v)| (*id, v)).collect();
        bulk_upsert_with_tape_split(
            self.pool(),
            self.tape_ref(),
            &rows,
            &tape_pairs,
            &volatile_pairs,
        )
        .await
    }

    pub async fn load_channels(&self) -> Result<Vec<Value>> {
        dr::load_payloads(self.pool(), "channels").await
    }

    pub async fn channels_for_fetch(
        &self,
        members_only: bool,
        include_archived: bool,
        include_dms: bool,
    ) -> Result<Vec<FetchTarget>> {
        let mut sql = String::from(
            "SELECT id, name, is_dm, dm_user_ids FROM channels WHERE payload IS NOT NULL",
        );
        if !include_dms {
            sql.push_str(" AND (is_dm IS NULL OR is_dm = 0)");
        }
        if members_only {
            // Channels only: a 1:1 DM has no `is_member` to be 1.
            sql.push_str(" AND (is_dm = 1 OR is_member = 1)");
        }
        if !include_archived {
            sql.push_str(" AND (is_archived IS NULL OR is_archived = 0)");
        }
        sql.push_str(" ORDER BY id");
        // Audited: `sql` is a static base with further `&'static str` clauses
        // appended by the `members_only` / `include_archived` flags.
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql))
            .fetch_all(self.pool())
            .await
            .context("select channels_for_fetch")?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let id: String = r.try_get("id").ok()?;
                Some(FetchTarget {
                    id,
                    name: r.try_get("name").ok().flatten(),
                    is_dm: r.try_get::<Option<i64>, _>("is_dm").ok().flatten() == Some(1),
                    dm_user_ids: parse_dm_user_ids(
                        r.try_get::<Option<String>, _>("dm_user_ids")
                            .ok()
                            .flatten()
                            .as_deref(),
                    ),
                })
            })
            .collect())
    }

    /// Every mirrored user's ids and names, for labelling DMs and their
    /// progress lines. Typed columns only — no payload parse, since
    /// this runs before the walk on every DM-enabled run.
    pub async fn user_directory(&self) -> Result<Vec<UserDirectoryEntry>> {
        let rows = sqlx::query("SELECT id, name, real_name, display_name FROM users")
            .fetch_all(self.pool())
            .await
            .context("select user_directory")?;
        Ok(rows
            .into_iter()
            .filter_map(|r| {
                let id: String = r.try_get("id").ok()?;
                if id.is_empty() {
                    return None;
                }
                Some(UserDirectoryEntry {
                    id,
                    name: r.try_get("name").ok().flatten(),
                    real_name: r.try_get("real_name").ok().flatten(),
                    display_name: r.try_get("display_name").ok().flatten(),
                })
            })
            .collect())
    }

    // ── messages ────────────────────────────────────────────────────

    pub async fn upsert_messages(&self, inputs: &[MessageInput]) -> Result<()> {
        let page = prepare(inputs)?;
        let mut tx = self.pool().begin().await.context("begin messages")?;
        write_messages(&mut tx, &page).await?;
        tx.commit().await.context("commit messages")?;
        self.tape_messages(&page);
        Ok(())
    }

    /// One `conversations.history` page, in one transaction: its
    /// messages, an edge for each file they carry, the stretch of the
    /// channel the page `covered`, and the deletion of what it
    /// `enumerated` and did not return. Returns how many rows went.
    pub async fn store_history_page(
        &self,
        channel_id: &str,
        inputs: &[MessageInput],
        covered: Option<&Span>,
        enumerated: Option<&Enumerated>,
    ) -> Result<usize> {
        let page = prepare(inputs)?;
        let mut tx = self.pool().begin().await.context("begin history page")?;
        write_messages(&mut tx, &page).await?;
        if let Some(span) = covered {
            coverage::cover(&mut tx, &history_scope(channel_id), span.clone()).await?;
        }
        let pruned = match enumerated {
            Some(stretch) => {
                let returned: HashSet<&str> = inputs.iter().map(|m| m.ts.as_str()).collect();
                prune_enumerated(&mut tx, channel_id, stretch, &returned).await?
            }
            None => 0,
        };
        tx.commit().await.context("commit history page")?;
        self.tape_messages(&page);
        Ok(pruned)
    }

    /// One `search.messages` page, in one transaction: the thread roots
    /// read again because the page found a reply newer than the one they
    /// listed, and the stretch of reply time the page settled, for each
    /// conversation it searched.
    pub async fn store_reply_search_page(
        &self,
        roots: &[MessageInput],
        scopes: &[String],
        covered: Option<&Span>,
    ) -> Result<()> {
        let page = prepare(roots)?;
        let mut tx = self
            .pool()
            .begin()
            .await
            .context("begin reply search page")?;
        write_messages(&mut tx, &page).await?;
        if let Some(span) = covered {
            for scope in scopes {
                coverage::cover(&mut tx, scope, span.clone()).await?;
            }
        }
        tx.commit().await.context("commit reply search page")?;
        self.tape_messages(&page);
        Ok(())
    }

    /// The `latest_reply` each stored thread root lists, by the root's
    /// key. A key with no stored root is absent.
    pub async fn root_latest_replies(
        &self,
        keys: &[String],
    ) -> Result<HashMap<String, Option<String>>> {
        let mut out = HashMap::new();
        for key in keys {
            let row: Option<(String, Option<String>)> = sqlx::query_as(
                "SELECT id, json_extract(payload, '$.latest_reply') FROM messages \
                 WHERE id = ? AND is_thread_root = 1",
            )
            .bind(key)
            .fetch_optional(self.pool())
            .await
            .with_context(|| format!("read the root {key}"))?;
            if let Some((id, latest_reply)) = row {
                out.insert(id, latest_reply);
            }
        }
        Ok(out)
    }

    /// The threads of `channel_id` with replies, each at the version its
    /// stored root lists: the root's `latest_reply`, keyed like the root.
    /// Every stored root is listed, not only the ones this run's walk
    /// returned, so a root stored by a run that died before its replies
    /// is owed.
    pub async fn threads_listed(&self, channel_id: &str) -> Result<Vec<Listed>> {
        let rows: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT id, json_extract(payload, '$.latest_reply') FROM messages \
             WHERE channel_id = ? AND is_thread_root = 1 \
               AND json_extract(payload, '$.reply_count') > 0 \
             ORDER BY ts",
        )
        .bind(channel_id)
        .fetch_all(self.pool())
        .await
        .with_context(|| format!("list the threads of {channel_id}"))?;
        Ok(rows
            .into_iter()
            .map(|(key, latest_reply)| Listed::new(key, latest_reply))
            .collect())
    }

    /// Each thread `conversations.replies` returned whole, in the
    /// transaction the loop holds the threads in: its row, its messages,
    /// and the deletion of the stored replies the read did not return.
    /// Returns how many rows went.
    pub async fn store_threads(
        &self,
        tx: &mut Transaction<'_, Sqlite>,
        fetched: &[Fetched<Thread>],
    ) -> Result<usize> {
        let mut pruned = 0;
        for f in fetched {
            let Outcome::Got(thread) = &f.outcome else {
                continue;
            };
            sqlx::query("INSERT OR IGNORE INTO threads (id) VALUES (?)")
                .bind(&f.listed.key)
                .execute(&mut **tx)
                .await
                .with_context(|| format!("list the thread {}", f.listed.key))?;
            let page = prepare(&thread.rows)?;
            write_messages(tx, &page).await?;
            let returned: HashSet<String> = page
                .iter()
                .map(|p| p.row.id_and_payload.id.clone())
                .collect();
            let gone = datalib_etl::prune::prune_scope_in_tx(
                tx,
                MessageRow::TABLE,
                &[("thread_root_uuid", f.listed.key.as_str())],
                &returned,
            )
            .await?;
            if !gone.is_empty() {
                tracing::info!(
                    event = "slack_replies_pruned",
                    thread = %f.listed.key,
                    removed = gone.len(),
                    "these replies are gone from the thread Slack just returned whole",
                );
            }
            pruned += gone.len();
            self.tape_messages(&page);
        }
        Ok(pruned)
    }

    fn tape_messages(&self, page: &[Prepared<'_>]) {
        let Some(tape) = self.tape_ref() else { return };
        let rows: Vec<(&str, &Value)> = page
            .iter()
            .map(|p| (p.row.id_and_payload.id.as_str(), p.payload))
            .collect();
        let batch = EventBatch {
            table: MessageRow::TABLE,
            rows: &rows,
        };
        if let Err(e) = tape.append_batch(&batch) {
            tracing::error!(
                event = "event_tape_append_failed",
                table = MessageRow::TABLE,
                count = rows.len(),
                error = %format!("{e:#}"),
                "the messages are stored, but the event tape is missing their lines"
            );
        }
    }

    pub async fn count_messages(&self) -> Result<i64> {
        sqlx::query_scalar("SELECT COUNT(*) FROM messages WHERE payload IS NOT NULL")
            .fetch_one(self.pool())
            .await
            .context("count messages")
    }

    pub async fn load_messages(&self) -> Result<Vec<LoadedMessage>> {
        let rows = sqlx::query(
            "SELECT id, team_id, channel_id, ts, thread_ts, is_thread_root, user_id,
                    json(payload) AS payload
             FROM messages
             WHERE payload IS NOT NULL
             ORDER BY channel_id, ts",
        )
        .fetch_all(self.pool())
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

    // ── read states, bookmarks, saved items ─────────────────────────

    pub async fn upsert_read_states(&self, entries: &[Value]) -> Result<()> {
        let mut rows: Vec<ChannelReadStateRow> = Vec::with_capacity(entries.len());
        let mut tape_pairs: Vec<(&str, &Value)> = Vec::with_capacity(entries.len());
        let mut volatile_store: Vec<(&str, Value)> = Vec::with_capacity(entries.len());
        for entry in entries {
            let Some(id) = entry.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            let (base, volatile) = dr::split_volatile(entry, READ_STATE_VOLATILE_PATHS);
            rows.push(ChannelReadStateRow {
                id_and_payload: WirePayload {
                    id: id.to_string(),
                    payload: serde_json::to_string(&base).context("serialize read state")?,
                },
            });
            tape_pairs.push((id, entry));
            if let Some(v) = volatile {
                volatile_store.push((id, v));
            }
        }
        if rows.is_empty() {
            return Ok(());
        }
        let volatile_pairs: Vec<(&str, &Value)> =
            volatile_store.iter().map(|(id, v)| (*id, v)).collect();
        bulk_upsert_with_tape_split(
            self.pool(),
            self.tape_ref(),
            &rows,
            &tape_pairs,
            &volatile_pairs,
        )
        .await
    }

    /// Each conversation's read state with its volatile half laid back
    /// over it: the whole `client.counts` entry, keyed by conversation id.
    pub async fn load_read_states(&self) -> Result<Vec<Value>> {
        let rows = sqlx::query(
            "SELECT json(r.payload) AS payload, json(b.volatile_payload) AS volatile \
             FROM channel_read_states r \
             LEFT JOIN channel_read_states_bookkeeping b ON b.id = r.id \
             WHERE r.payload IS NOT NULL ORDER BY r.id",
        )
        .fetch_all(self.pool())
        .await
        .context("select channel_read_states")?;
        let mut out = Vec::with_capacity(rows.len());
        for r in rows {
            let parse = |col: &str| {
                r.try_get::<Option<String>, _>(col)
                    .ok()
                    .flatten()
                    .and_then(|s| serde_json::from_str::<Value>(&s).ok())
            };
            let Some(base) = parse("payload") else {
                continue;
            };
            out.push(match parse("volatile") {
                Some(v) => dr::overlay(&base, &v),
                None => base,
            });
        }
        Ok(out)
    }

    /// Store one conversation's `bookmarks.list` answer and drop the
    /// bookmarks it no longer names: the listing is not paged, so it is
    /// the conversation's whole set. Returns how many went.
    pub async fn replace_bookmarks(&self, channel_id: &str, payloads: &[Value]) -> Result<usize> {
        let mut rows: Vec<BookmarkRow> = Vec::with_capacity(payloads.len());
        let mut tape_pairs: Vec<(&str, &Value)> = Vec::with_capacity(payloads.len());
        for payload in payloads {
            let Some(id) = payload.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            rows.push(BookmarkRow {
                id_and_payload: WirePayload {
                    id: id.to_string(),
                    payload: serde_json::to_string(payload).context("serialize bookmark")?,
                },
                channel_id: channel_id.to_string(),
            });
            tape_pairs.push((id, payload));
        }
        let keep: HashSet<String> = rows.iter().map(|r| r.id_and_payload.id.clone()).collect();
        if !rows.is_empty() {
            bulk_upsert_with_tape(self.pool(), self.tape_ref(), &rows, &tape_pairs).await?;
        }
        let gone = datalib_etl::prune::prune_scope(
            self.pool(),
            "bookmarks",
            &[("channel_id", channel_id)],
            &keep,
        )
        .await?;
        Ok(gone.len())
    }

    /// Conversations whose header shows a bookmarks bar or a bookmark
    /// folder (`properties.tabs` of type `bookmarks` / `folder`), plus
    /// every one we already hold bookmarks for, so a bar emptied upstream
    /// is still asked about once more.
    pub async fn channels_to_list_bookmarks(&self) -> Result<HashSet<String>> {
        let ids: Vec<String> = sqlx::query_scalar(
            "SELECT c.id FROM channels c, json_each(c.payload, '$.properties.tabs') t \
             WHERE json_extract(t.value, '$.type') IN ('bookmarks', 'folder') \
             UNION SELECT channel_id FROM bookmarks",
        )
        .fetch_all(self.pool())
        .await
        .context("select channels with a bookmarks tab")?;
        Ok(ids.into_iter().collect())
    }

    pub async fn load_bookmarks(&self) -> Result<Vec<Value>> {
        dr::load_payloads(self.pool(), "bookmarks").await
    }

    /// Store a complete `saved.list` walk and drop the stored items it no
    /// longer names — but only those in `in_scope` conversations. An item
    /// in a conversation this run does not mirror was not asked about, so
    /// it is left alone. Returns how many went.
    pub async fn replace_saved_items(
        &self,
        items: &[Value],
        in_scope: &HashSet<String>,
    ) -> Result<usize> {
        let mut rows: Vec<SavedItemRow> = Vec::with_capacity(items.len());
        let mut tape_pairs: Vec<(String, &Value)> = Vec::with_capacity(items.len());
        for item in items {
            let text = |k: &str| item.get(k).and_then(|v| v.as_str());
            let (Some(item_type), Some(item_id)) = (text("item_type"), text("item_id")) else {
                continue;
            };
            let ts = text("ts");
            let id = saved_item_key(item_type, item_id, ts);
            rows.push(SavedItemRow {
                id_and_payload: WirePayload {
                    id: id.clone(),
                    payload: serde_json::to_string(item).context("serialize saved item")?,
                },
                item_type: item_type.to_string(),
                item_id: item_id.to_string(),
                ts: ts.map(String::from),
            });
            tape_pairs.push((id, item));
        }
        if !rows.is_empty() {
            let pairs: Vec<(&str, &Value)> =
                tape_pairs.iter().map(|(id, v)| (id.as_str(), *v)).collect();
            bulk_upsert_with_tape(self.pool(), self.tape_ref(), &rows, &pairs).await?;
        }

        let stored: Vec<(String, String)> = sqlx::query_as("SELECT id, item_id FROM saved_items")
            .fetch_all(self.pool())
            .await
            .context("select saved_items")?;
        let mut keep: HashSet<String> = rows.iter().map(|r| r.id_and_payload.id.clone()).collect();
        keep.extend(
            stored
                .iter()
                .filter(|(_, item_id)| !in_scope.contains(item_id))
                .map(|(id, _)| id.clone()),
        );
        let gone = datalib_etl::prune::prune_scope(self.pool(), "saved_items", &[], &keep).await?;
        datalib_etl::prune::record("slack saved items", stored.len(), gone.len());
        Ok(gone.len())
    }

    pub async fn load_saved_items(&self) -> Result<Vec<Value>> {
        dr::load_payloads(self.pool(), "saved_items").await
    }

    // ── attachments (per-provider CAS edge) ─────────────────────────

    /// Snapshot `(file_id → blake3)` for every attachment whose bytes
    /// have ever landed in the CAS. Called once at the start of a
    /// fetch run so the per-file "have we got these bytes yet?"
    /// check is a HashMap hit instead of a SQLite round trip queued
    /// behind preceding multi-MB CAS commits on the single-connection
    /// doltlite pool.
    pub async fn load_attachment_blake3s(&self) -> Result<HashMap<String, String>> {
        datalib_etl::blob_cas::load_blake3_index(self.pool(), "slack_attachments", "file_id").await
    }

    /// The files of `channel_id` whose bytes the store does not hold: an
    /// edge with no `blake3`. An edge's key starts with its message's,
    /// and a message's with its channel's, so one channel's edges are a
    /// range of keys. A file has no version: it only has to land.
    pub async fn files_listed(&self, team_id: &str, channel_id: &str) -> Result<Vec<Listed>> {
        let from = slack_message_key(team_id, channel_id, "");
        // `$` is the character after `#`.
        let to = format!("{team_id}#{channel_id}$");
        let keys: Vec<String> = sqlx::query_scalar(
            "SELECT id FROM slack_attachments WHERE id >= ? AND id < ? AND blake3 IS NULL \
             ORDER BY id",
        )
        .bind(&from)
        .bind(&to)
        .fetch_all(self.pool())
        .await
        .with_context(|| format!("list the files of {channel_id}"))?;
        Ok(keys
            .into_iter()
            .map(|key| Listed::new(key, None::<String>))
            .collect())
    }

    /// The file object each of `keys` names, as its stored message
    /// carries it: what a fetch asks Slack with. An edge whose message no
    /// longer carries the file is left out.
    pub async fn file_objects(&self, keys: &[&str]) -> Result<Vec<OwedFile>> {
        let mut out = Vec::with_capacity(keys.len());
        for chunk in keys.chunks(datalib_etl::bulk::SQL_CHUNK) {
            // Audited: the placeholders are one `?` per key, every key
            // bound.
            let sql = format!(
                "SELECT a.id, a.message_uuid, json(f.value) AS file \
                 FROM slack_attachments a \
                 JOIN messages m ON m.id = a.message_uuid \
                 JOIN json_each(m.payload, '$.files') f \
                   ON json_extract(f.value, '$.id') = a.file_id \
                 WHERE a.id IN ({}) ORDER BY a.id",
                vec!["?"; chunk.len()].join(",")
            );
            let mut q = sqlx::query_as::<_, (String, String, String)>(sqlx::AssertSqlSafe(sql));
            for key in chunk {
                q = q.bind(*key);
            }
            let rows = q
                .fetch_all(self.pool())
                .await
                .context("read the file objects of a batch")?;
            for (key, message_uuid, file) in rows {
                out.push(OwedFile {
                    key,
                    message_uuid,
                    file: serde_json::from_str(&file)?,
                });
            }
        }
        Ok(out)
    }
}

/// The `coverage` scope of one channel's history.
pub fn history_scope(channel_id: &str) -> String {
    format!("history:{channel_id}")
}

/// The `coverage` scope of the reply time searched in one conversation,
/// as `ts` keys.
pub fn replies_scope(channel_id: &str) -> String {
    format!("replies:{channel_id}")
}

/// A stretch of a channel one `conversations.history` page listed whole,
/// as message `ts`es: a stored top-level message inside it that the page
/// did not return is gone upstream.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enumerated {
    pub oldest: String,
    pub latest: String,
    /// Whether a message at `latest` itself is this page's to return, or
    /// was the page before's.
    pub latest_included: bool,
}

/// What one `conversations.replies` read of a thread returned, root
/// copy included.
#[derive(Debug)]
pub struct Thread {
    pub rows: Vec<MessageInput>,
}

/// One edge without bytes and the file object its message carries.
#[derive(Debug, Clone, PartialEq)]
pub struct OwedFile {
    /// The edge's key.
    pub key: String,
    pub message_uuid: String,
    pub file: Value,
}

struct Prepared<'a> {
    row: MessageRow,
    payload: &'a Value,
    volatile: Option<Value>,
}

fn prepare(inputs: &[MessageInput]) -> Result<Vec<Prepared<'_>>> {
    inputs
        .iter()
        .map(|m| {
            let effective_thread_ts = m.thread_ts.as_deref().unwrap_or(m.ts.as_str());
            let (base, volatile) = dr::split_volatile(&m.payload, MESSAGE_VOLATILE_PATHS);
            Ok(Prepared {
                row: MessageRow {
                    id_and_payload: WirePayload {
                        id: slack_message_key(&m.team_id, &m.channel_id, &m.ts),
                        payload: serde_json::to_string(&base).context("serialize message")?,
                    },
                    team_id: m.team_id.clone(),
                    channel_id: m.channel_id.clone(),
                    ts: m.ts.clone(),
                    thread_ts: m.thread_ts.clone(),
                    thread_root_uuid: slack_thread_key(
                        &m.team_id,
                        &m.channel_id,
                        effective_thread_ts,
                    ),
                    is_thread_root: m.is_thread_root as i64,
                    user_id: m.user_id.clone(),
                },
                payload: &m.payload,
                volatile,
            })
        })
        .collect()
}

/// The messages, and an edge with no bytes for each file one carries
/// that Slack serves. An edge already there is left alone: it may hold
/// the bytes.
async fn write_messages(tx: &mut Transaction<'_, Sqlite>, page: &[Prepared<'_>]) -> Result<()> {
    let rows: Vec<MessageRow> = page.iter().map(|p| p.row.clone()).collect();
    let volatile: Vec<(&str, &Value)> = page
        .iter()
        .filter_map(|p| Some((p.row.id_and_payload.id.as_str(), p.volatile.as_ref()?)))
        .collect();
    let now = datalib_time::IsoOffsetTimestamp::now_local();
    bulk_upsert_in_tx(tx, &rows, &now).await?;
    dr::merge_volatile_payloads_in_tx(tx, MessageRow::TABLE, &volatile).await?;
    for p in page {
        let message_uuid = &p.row.id_and_payload.id;
        let files = p.payload.get("files").and_then(Value::as_array);
        for file_id in files
            .into_iter()
            .flatten()
            .filter_map(super::api::served_file_id)
        {
            let id = SlackAttachmentRow::pk_recipe(message_uuid, file_id);
            sqlx::query(
                "INSERT INTO slack_attachments (id, message_uuid, file_id) VALUES (?, ?, ?) \
                 ON CONFLICT(id) DO NOTHING",
            )
            .bind(&id)
            .bind(message_uuid)
            .bind(file_id)
            .execute(&mut **tx)
            .await
            .with_context(|| format!("list the file {id}"))?;
            sqlx::query(
                "INSERT INTO slack_attachments_bookkeeping (id, attempt_count) VALUES (?, 0) \
                 ON CONFLICT(id) DO NOTHING",
            )
            .bind(&id)
            .execute(&mut **tx)
            .await
            .with_context(|| format!("list the file {id}"))?;
        }
    }
    Ok(())
}

/// Delete the stored top-level messages of `stretch` that are not in
/// `returned`, each with the replies of its thread and the thread's own
/// row. Only rows history could have returned are judged: it lists a
/// thread's root and never its replies, so a reply goes only with its
/// root, since nothing asks for the replies of a root no longer listed.
async fn prune_enumerated(
    tx: &mut Transaction<'_, Sqlite>,
    channel_id: &str,
    stretch: &Enumerated,
    returned: &HashSet<&str>,
) -> Result<usize> {
    let top_level: Vec<(String, String)> = sqlx::query_as(
        "SELECT ts, thread_root_uuid FROM messages \
         WHERE channel_id = ? AND ts >= ? AND (ts < ? OR (? AND ts = ?)) AND is_thread_root = 1",
    )
    .bind(channel_id)
    .bind(&stretch.oldest)
    .bind(&stretch.latest)
    .bind(stretch.latest_included)
    .bind(&stretch.latest)
    .fetch_all(&mut **tx)
    .await
    .with_context(|| format!("list stored messages in a stretch of {channel_id}"))?;

    let gone_threads: Vec<&String> = top_level
        .iter()
        .filter(|(ts, _)| !returned.contains(ts.as_str()))
        .map(|(_, thread)| thread)
        .collect();
    let mut gone = 0;
    let none = HashSet::new();
    for thread in &gone_threads {
        let scope = [("thread_root_uuid", thread.as_str())];
        let messages =
            datalib_etl::prune::prune_scope_in_tx(tx, MessageRow::TABLE, &scope, &none).await?;
        datalib_etl::prune::delete_owned_in_tx(
            tx,
            SlackAttachmentRow::TABLE,
            "message_uuid",
            &messages,
        )
        .await?;
        let own = [("id", thread.as_str())];
        datalib_etl::prune::prune_scope_in_tx(tx, THREADS, &own, &none).await?;
        gone += messages.len();
    }
    datalib_etl::prune::record(
        &format!("slack channel {channel_id} history"),
        top_level.len(),
        gone_threads.len(),
    );
    Ok(gone)
}

/// Participant ids out of one conversation payload. An `im` names
/// exactly one counterpart in `user`; an `mpim` lists everyone — the
/// account included — in `members`. Verified against the live API
/// 2026-08-31; see [`super::schema_raw::ChannelRow::dm_user_ids`].
pub fn dm_participants(payload: &Value, is_im: bool) -> Vec<String> {
    if is_im {
        return payload
            .get("user")
            .and_then(|v| v.as_str())
            .map(|u| vec![u.to_string()])
            .unwrap_or_default();
    }
    payload
        .get("members")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|v| v.as_str())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

/// One conversation the fetch run may walk, as
/// [`RawDb::channels_for_fetch`] found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchTarget {
    pub id: String,
    /// `#general`'s bare name, or an `mpim`'s `mpdm-…` composite
    /// handle. Always `None` for a 1:1 DM — Slack gives it no name.
    pub name: Option<String>,
    pub is_dm: bool,
    /// Who is in this DM, as Slack listed them — self included for an
    /// `mpim`. Empty for a channel. See
    /// [`super::schema_raw::ChannelRow::dm_user_ids`].
    pub dm_user_ids: Vec<String>,
}

/// One user's ids and names, for labelling DMs.
#[derive(Debug, Clone)]
pub struct UserDirectoryEntry {
    pub id: String,
    pub name: Option<String>,
    pub real_name: Option<String>,
    pub display_name: Option<String>,
}

impl UserDirectoryEntry {
    pub fn label(&self) -> String {
        crate::user_label(self.real_name.as_deref(), self.name.as_deref(), &self.id)
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
/// the render path needs at hand.
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

/// Bag returned to the synchronous render path. No `BlobReader`
/// here — render attaches per-thread [`BlobBundle`]s separately via
/// `parse_doltlite_async`.
#[derive(Clone, Default)]
pub struct LoadedRaw {
    pub workspace: Option<Value>,
    pub users: Vec<Value>,
    pub channels: Vec<Value>,
    pub messages: Vec<LoadedMessage>,
}

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
    use crate::ingest::schema_raw::split_key;
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
        let mem_only = db.channels_for_fetch(true, false, false).await.unwrap();
        assert_eq!(ids(&mem_only), vec!["C1"]);
        let with_archived = db.channels_for_fetch(true, true, false).await.unwrap();
        assert_eq!(ids(&with_archived), vec!["C1", "C3"]);
    }

    fn ids(targets: &[FetchTarget]) -> Vec<&str> {
        targets.iter().map(|t| t.id.as_str()).collect()
    }

    /// The trap this column exists to avoid: a DM has no `is_member`
    /// and no `is_archived`, so the channel predicates would reject
    /// every one of them regardless of what the config asked for.
    #[tokio::test]
    async fn dms_are_selected_by_their_own_predicate() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("s.doltlite_db")).await.unwrap();
        db.upsert_channel(&json!({"id": "C1", "name": "a", "is_member": true}))
            .await
            .unwrap();
        // Exactly what conversations.list returns for a 1:1 DM: no
        // name, no is_member, no is_archived.
        db.upsert_channel(&json!({"id": "D1", "is_im": true, "user": "U2", "is_archived": false}))
            .await
            .unwrap();
        // A live mpim looks like a private channel that also carries
        // `members` — including the account itself.
        db.upsert_channel(&json!({
            "id": "G1", "is_mpim": true, "is_member": true, "is_archived": false,
            "name": "mpdm-a--b--c-1", "members": ["U1", "U2", "U3"],
        }))
        .await
        .unwrap();

        // DMs off: the channel filters apply and nothing else appears.
        assert_eq!(
            ids(&db.channels_for_fetch(true, false, false).await.unwrap()),
            vec!["C1"]
        );

        // DMs on: both surfaces come back, and `members_only` does not
        // suppress them.
        let with_dms = db.channels_for_fetch(true, false, true).await.unwrap();
        assert_eq!(ids(&with_dms), vec!["C1", "D1", "G1"]);
        let im = with_dms.iter().find(|t| t.id == "D1").unwrap();
        assert!(im.is_dm);
        assert_eq!(im.dm_user_ids, vec!["U2".to_string()]);
        assert_eq!(im.name, None);
        let mpim = with_dms.iter().find(|t| t.id == "G1").unwrap();
        assert!(mpim.is_dm);
        // A group DM's participants come from `members`, stored
        // verbatim — self included, subtracted at read time.
        assert_eq!(mpim.dm_user_ids, vec!["U1", "U2", "U3"]);
        assert_eq!(mpim.name.as_deref(), Some("mpdm-a--b--c-1"));
        let channel = with_dms.iter().find(|t| t.id == "C1").unwrap();
        assert!(!channel.is_dm);
        assert!(channel.dm_user_ids.is_empty());
    }

    #[tokio::test]
    async fn user_directory_round_trips() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("s.doltlite_db")).await.unwrap();
        db.upsert_users(&[
            json!({"id": "U1", "name": "picard", "real_name": "Jean-Luc Picard"}),
            json!({"id": "U2", "name": "riker", "profile": {"display_name": "Number One"}}),
        ])
        .await
        .unwrap();
        let dir = db.user_directory().await.unwrap();
        assert_eq!(dir.len(), 2);
        let u1 = dir.iter().find(|u| u.id == "U1").unwrap();
        assert_eq!(u1.label(), "Jean-Luc Picard");
        let u2 = dir.iter().find(|u| u.id == "U2").unwrap();
        assert_eq!(u2.display_name.as_deref(), Some("Number One"));
        // No real_name: falls back to the handle, same as render's User::label.
        assert_eq!(u2.label(), "riker");
    }

    fn root(ts: &str, replies: u64, latest_reply: Option<&str>) -> MessageInput {
        let mut payload = json!({"ts": ts, "thread_ts": ts, "reply_count": replies});
        if let Some(latest) = latest_reply {
            payload["latest_reply"] = json!(latest);
        }
        MessageInput {
            team_id: "T1".into(),
            channel_id: "C1".into(),
            ts: ts.into(),
            thread_ts: Some(ts.into()),
            is_thread_root: true,
            user_id: None,
            payload,
        }
    }

    async fn owed_threads(db: &RawDb) -> Vec<String> {
        let listed = db.threads_listed("C1").await.unwrap();
        datalib_etl_web::owed::owed(db.pool(), THREADS, listed)
            .await
            .unwrap()
            .into_iter()
            .map(|l| split_key(&l.key).unwrap().2.to_string())
            .collect()
    }

    /// A read of the thread returning its root copy and no reply, held
    /// at `latest_reply`: what the loop does for a thread that came
    /// whole.
    async fn read_thread(db: &RawDb, ts: &str, latest_reply: &str) {
        let key = slack_message_key("T1", "C1", ts);
        let fetched = Fetched {
            listed: Listed::new(key.clone(), Some(latest_reply)),
            outcome: Outcome::Got(Thread {
                rows: vec![root(ts, 2, Some(latest_reply))],
            }),
        };
        let mut tx = db.pool().begin().await.unwrap();
        db.store_threads(&mut tx, std::slice::from_ref(&fetched))
            .await
            .unwrap();
        datalib_etl_web::owed::hold(&mut tx, THREADS, &key, Some(latest_reply))
            .await
            .unwrap();
        tx.commit().await.unwrap();
    }

    /// A stored root with replies is owed with no run having "listed" it,
    /// until it is held at the `latest_reply` it lists; and again when
    /// the root is stored listing a newer one.
    #[tokio::test]
    async fn a_thread_is_owed_while_its_root_lists_a_version_it_is_not_held_at() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("s.doltlite_db")).await.unwrap();
        db.upsert_messages(&[
            root("1.0", 2, Some("3.0")),
            root("5.0", 0, None),
            root("6.0", 1, Some("7.0")),
        ])
        .await
        .unwrap();
        assert_eq!(owed_threads(&db).await, ["1.0", "6.0"]);

        read_thread(&db, "1.0", "3.0").await;
        assert_eq!(owed_threads(&db).await, ["6.0"]);

        db.upsert_messages(&[root("1.0", 3, Some("4.0"))])
            .await
            .unwrap();
        assert_eq!(owed_threads(&db).await, ["1.0", "6.0"]);
    }

    /// A failed read is an attempt on the thread's own row, so storing
    /// the root again (a refresh does) neither clears it nor the thread's
    /// place among the owed; the next whole read clears both.
    #[tokio::test]
    async fn a_failed_replies_read_stays_on_the_thread_until_it_is_read() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("s.doltlite_db")).await.unwrap();
        let thread = root("1.0", 2, Some("3.0"));
        db.upsert_messages(std::slice::from_ref(&thread))
            .await
            .unwrap();
        let key = slack_message_key("T1", "C1", "1.0");
        let mut tx = db.pool().begin().await.unwrap();
        dr::record_object_error(&mut tx, THREADS, &key, "internal_error")
            .await
            .unwrap();
        tx.commit().await.unwrap();
        db.upsert_messages(std::slice::from_ref(&thread))
            .await
            .unwrap();

        let problems = || async {
            sqlx::query_scalar::<_, String>("SELECT scope_key FROM problems")
                .fetch_all(db.pool())
                .await
                .unwrap()
        };
        assert_eq!(problems().await, ["threads:T1#C1#1.0"]);
        assert_eq!(owed_threads(&db).await, ["1.0"]);

        read_thread(&db, "1.0", "3.0").await;
        assert!(problems().await.is_empty());
        assert!(owed_threads(&db).await.is_empty());
    }

    fn with_block_id(mut message: MessageInput, block_id: &str) -> MessageInput {
        message.payload["blocks"] = json!([{
            "type": "rich_text", "block_id": block_id,
            "elements": [{"type": "rich_text_section",
                          "elements": [{"type": "text", "text": "status report"}]}],
        }]);
        message
    }

    async fn message_payload_and_volatile(db: &RawDb, ts: &str) -> (String, Option<Value>) {
        let key = slack_message_key("T1", "C1", ts);
        let payload: String = sqlx::query_scalar("SELECT json(payload) FROM messages WHERE id = ?")
            .bind(&key)
            .fetch_one(db.pool())
            .await
            .unwrap();
        let volatile: Option<String> = sqlx::query_scalar(
            "SELECT json(volatile_payload) FROM messages_bookkeeping WHERE id = ?",
        )
        .bind(&key)
        .fetch_one(db.pool())
        .await
        .unwrap();
        (payload, volatile.map(|v| serde_json::from_str(&v).unwrap()))
    }

    /// Slack mints a fresh `block_id` for a rich-text block on every
    /// read, so a message read twice must store the same content row.
    #[tokio::test]
    async fn a_re_minted_block_id_is_not_a_change_to_the_message() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("s.doltlite_db")).await.unwrap();
        db.upsert_messages(&[with_block_id(root("1.0", 0, None), "Ab1")])
            .await
            .unwrap();
        let (first, _) = message_payload_and_volatile(&db, "1.0").await;

        db.upsert_messages(&[with_block_id(root("1.0", 0, None), "Zz9")])
            .await
            .unwrap();
        let (second, volatile) = message_payload_and_volatile(&db, "1.0").await;

        assert_eq!(first, second);
        assert!(!second.contains("block_id"), "{second}");
        assert_eq!(volatile.unwrap()["blocks"][0]["block_id"], "Zz9");
    }

    /// The history copy of a followed root has block ids to split but no
    /// read mark; storing it after the replies copy (a refresh does) must
    /// not take the mark away from the thread.
    #[tokio::test]
    async fn the_history_copy_of_a_followed_root_keeps_its_read_mark() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("s.doltlite_db")).await.unwrap();
        let mut replies_copy = with_block_id(root("1.0", 1, Some("2.0")), "Ab1");
        replies_copy.payload["last_read"] = json!("1.0");
        replies_copy.payload["subscribed"] = json!(true);
        db.upsert_messages(&[replies_copy]).await.unwrap();
        let (first, _) = message_payload_and_volatile(&db, "1.0").await;

        let history_copy = with_block_id(root("1.0", 1, Some("2.0")), "Zz9");
        db.upsert_messages(&[history_copy]).await.unwrap();
        let (second, volatile) = message_payload_and_volatile(&db, "1.0").await;

        assert_eq!(first, second);
        assert_eq!(
            volatile.unwrap(),
            json!({"last_read": "1.0", "subscribed": true,
                   "blocks": [{"block_id": "Zz9"}]})
        );
    }

    fn with_file(ts: &str, file: Value) -> MessageInput {
        MessageInput {
            team_id: "T1".into(),
            channel_id: "C1".into(),
            ts: ts.into(),
            thread_ts: None,
            is_thread_root: true,
            user_id: None,
            payload: json!({"ts": ts, "files": [file]}),
        }
    }

    /// A file is listed, as an edge without bytes, in the transaction
    /// that stores its message, so it is owed from then on; one Slack
    /// does not serve is not listed; and storing the message again does
    /// not take the bytes an edge already points at.
    #[tokio::test]
    async fn a_file_is_owed_from_the_moment_its_message_is_stored() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("s.doltlite_db")).await.unwrap();
        let log = json!({"id": "F1", "url_private": "https://files.slack.com/F1"});
        let messages = [
            with_file("1.0", log.clone()),
            with_file("2.0", json!({"id": "F2", "mode": "tombstone"})),
        ];
        db.upsert_messages(&messages).await.unwrap();
        let listed = db.files_listed("T1", "C1").await.unwrap();
        assert_eq!(listed, [Listed::new("T1#C1#1.0#F1", None::<String>)]);
        assert_eq!(
            db.file_objects(&["T1#C1#1.0#F1"]).await.unwrap(),
            [OwedFile {
                key: "T1#C1#1.0#F1".into(),
                message_uuid: "T1#C1#1.0".into(),
                file: log,
            }]
        );
        assert!(db.files_listed("T1", "C10").await.unwrap().is_empty());

        let hash = "a".repeat(64);
        sqlx::query("UPDATE slack_attachments SET blake3 = ? WHERE file_id = 'F1'")
            .bind(&hash)
            .execute(db.pool())
            .await
            .unwrap();
        db.upsert_messages(&messages).await.unwrap();
        assert!(db.files_listed("T1", "C1").await.unwrap().is_empty());
    }

    fn id_set(values: &[Value], key: &str) -> Vec<String> {
        let mut out: Vec<String> = values
            .iter()
            .map(|v| v[key].as_str().unwrap().to_string())
            .collect();
        out.sort();
        out
    }

    /// A conversation's bookmark listing is its whole set: one gone
    /// upstream goes here, and another conversation's are untouched.
    #[tokio::test]
    async fn replace_bookmarks_prunes_within_its_conversation() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("s.doltlite_db")).await.unwrap();
        let bm = |id: &str| json!({"id": id, "type": "link"});
        db.replace_bookmarks("C1", &[bm("B1"), bm("B2")])
            .await
            .unwrap();
        db.replace_bookmarks("C2", &[bm("B3")]).await.unwrap();

        let gone = db.replace_bookmarks("C1", &[bm("B2")]).await.unwrap();

        assert_eq!(gone, 1);
        assert_eq!(
            id_set(&db.load_bookmarks().await.unwrap(), "id"),
            vec!["B2", "B3"]
        );
    }

    /// A saved item in a conversation this run does not mirror was not
    /// asked about, so its absence from the listing deletes nothing.
    #[tokio::test]
    async fn replace_saved_items_prunes_only_in_scope() {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("s.doltlite_db")).await.unwrap();
        let item = |cid: &str, ts: &str| json!({"item_type": "message", "item_id": cid, "ts": ts});
        let both: HashSet<String> = ["C1", "C2"].map(String::from).into();
        db.replace_saved_items(&[item("C1", "1.0"), item("C2", "2.0")], &both)
            .await
            .unwrap();

        // `C2` has left the config; `C1`'s item was unsaved upstream.
        let only_c1: HashSet<String> = ["C1".to_string()].into();
        let gone = db.replace_saved_items(&[], &only_c1).await.unwrap();

        assert_eq!(gone, 1);
        assert_eq!(
            id_set(&db.load_saved_items().await.unwrap(), "item_id"),
            vec!["C2"]
        );
    }
}
