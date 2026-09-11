//! Doltlite-aware parse entry point. Two-phase:

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use datalib_etl::blob_cas::{self, BlobBundle};
use serde_json::Value;
use sqlx::sqlite::SqlitePool;
use sqlx::Row;

use datalib_etl_slack::ingest::db::db_path_for;
use datalib_etl_slack::ingest::schema_raw::slack_thread_uuid;
use datalib_etl_slack::ingest::shapes::{M_AUTH_TEST, M_CHANNELS, M_HISTORY, M_REPLIES, M_USERS};

use super::{ts_to_iso, Channel, Message, User, Workspace};

/// SQL projection that maps a Slack `file_id` to its CAS blake3.
/// Used by [`BlobBundle::load`] from the per-thread load below.
const ATTACHMENTS_PROJECTION_SQL: &str = "
    SELECT file_id AS ref_id, MAX(blake3) AS blake3,
           NULL AS content_type, NULL AS upstream_name
      FROM pinned_slack_attachments slack_attachments
     WHERE file_id IN ({placeholders}) AND blake3 IS NOT NULL
     GROUP BY file_id";

/// Result of the dolt_diff scan. Travels alongside the parsed bag so
/// render can advance the cursor + log timing without a second round
/// trip.
#[derive(Debug, Clone, Default)]
pub struct ScanResult {
    /// `Some(set)` → render only threads whose `thread_root_uuid` is
    /// in `set`. `None` → cold start, render everything.
    pub changed_threads: Option<HashSet<String>>,
    /// HEAD commit hash at scan time, ready to stamp into the render
    /// cursor on success.
    pub new_head: Option<String>,
    /// Wall-clock time spent in the union query. `None` on cold start.
    pub scan_elapsed: Option<Duration>,
}

/// One thread as it sits between parse and render: the messages
/// belonging to this thread plus the attachment bytes they reference.
#[derive(Debug, Clone)]
pub struct SlackThreadBucket {
    pub thread_uuid: String,
    pub messages: Vec<Message>,
    pub blobs: BlobBundle,
}

#[derive(Default)]
pub struct ParsedSlack {
    pub workspace: Option<Workspace>,
    pub users: BTreeMap<String, User>,
    pub channels: BTreeMap<String, Channel>,
    pub threads: Vec<SlackThreadBucket>,
    /// Count of threads `dolt_diff` reported as unchanged. Reported
    /// into the render summary so the orchestrator's progress
    /// accounting stays accurate.
    pub docs_skipped: usize,
    /// Scan diagnostics propagated up to render so it can write the
    /// cursor + log elapsed_ms.
    pub scan: ScanResult,
    /// Bucket keys the diff named that the raw store no longer has a row
    /// for. Empty on a cold start, which looks at every bucket and so has
    /// nothing to compare against.
    pub vanished_buckets: Vec<String>,
}

impl ParsedSlack {
    /// What the grid's Account column shows: the login `auth.test`
    /// named, resolved through its own `users` row — email, else real
    /// name, else handle, else the bare user id. `None` when the store
    /// has no workspace row at all.
    pub fn account_label(&self) -> Option<String> {
        let ws = self.workspace.as_ref()?;
        let self_id = ws.self_user_id.as_deref()?;
        let user = self.users.get(self_id);
        datalib_etl_chat_common::account_label(
            self_id,
            user.and_then(|u| u.email.as_deref()),
            user.map(|u| u.label()).as_deref(),
        )
    }
}

pub fn parse(path: &Path, last_render_hash: Option<&str>) -> Result<ParsedSlack> {
    let db_path = db_path_for(path);
    if db_path.exists() {
        return parse_doltlite(&db_path, last_render_hash);
    }
    if path.is_dir() {
        return parse_raw_json_dir(path);
    }
    // No store and no legacy tree: this source has never been
    // downloaded. That is the normal state of every source in a
    // freshly scaffolded config, not an error — render nothing and
    // succeed. A store that exists but can't be read still fails
    // above. See docs/dev/step_protocol.md, "Rendering a source with
    // no data".
    Ok(ParsedSlack::default())
}

fn parse_doltlite(db_path: &Path, last_render_hash: Option<&str>) -> Result<ParsedSlack> {
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current()
            .block_on(async move { parse_doltlite_async(db_path, last_render_hash).await })
    })
}

async fn parse_doltlite_async(
    db_path: &Path,
    last_render_hash: Option<&str>,
) -> Result<ParsedSlack> {
    let pool = datalib_etl::doltlite_raw::open_reader(db_path)
        .await
        .with_context(|| format!("open slack doltlite for render {}", db_path.display()))?;

    let cas_path = blob_cas::cas_path_for(db_path);
    let cas_pool: Option<SqlitePool> = if cas_path.is_file() {
        Some(
            datalib_etl::doltlite_raw::open_reader(&cas_path)
                .await
                .with_context(|| format!("open slack CAS for render {}", cas_path.display()))?,
        )
    } else {
        None
    };

    // Pin before anything reads this store. The diff below and the rows
    // behind it have to name one commit, and the `pinned_<table>` views must
    // already exist when the diff runs — its bucket query joins live tables.
    // No commit at all means nothing has been committed here to render, which
    // is emptiness, not a reason to read the working set.

    let Some(pin) = datalib_etl::pin::head(&pool).await? else {
        return Ok(ParsedSlack::default());
    };

    datalib_etl::pin::install_views(&pool, &pin)
        .await
        .context("pin the slack raw store for render")?;

    let scan = scan_diff(&pool, last_render_hash, &pin).await?;

    // Workspace + users + channels are cheap and shared across threads.
    let workspace = load_workspace(&pool).await?;
    let users = load_users(&pool).await?;
    let channels = load_channels(&pool).await?;

    // Load messages. When the scan narrowed the set, load only those
    // threads' messages; otherwise load everything.
    let total_threads = thread_count(&pool).await?;
    let (messages, docs_skipped) = match &scan.changed_threads {
        None => (load_all_messages(&pool).await?, 0usize),
        Some(changed) => {
            let kept = load_messages_for_threads(&pool, changed).await?;
            let touched_threads: HashSet<&str> =
                kept.iter().map(|m| m.thread_root_uuid.as_str()).collect();
            let skipped = total_threads.saturating_sub(touched_threads.len());
            (kept, skipped)
        }
    };

    // Group messages into thread buckets.
    let team_id = workspace
        .as_ref()
        .map(|w| w.team_id.clone())
        .unwrap_or_else(|| "unknown".into());
    let mut by_thread: BTreeMap<String, Vec<Message>> = BTreeMap::new();
    for m in messages {
        let msg = loaded_to_message(&m, &team_id);
        by_thread
            .entry(m.thread_root_uuid.clone())
            .or_default()
            .push(msg);
    }
    let mut threads: Vec<SlackThreadBucket> = Vec::with_capacity(by_thread.len());
    for (thread_uuid, mut msgs) in by_thread {
        msgs.sort_by(|a, b| {
            (a.ts_iso.as_deref(), a.ts.as_str()).cmp(&(b.ts_iso.as_deref(), b.ts.as_str()))
        });
        threads.push(SlackThreadBucket {
            thread_uuid,
            messages: msgs,
            blobs: BlobBundle::default(),
        });
    }

    // Per-thread BlobBundle: walk each thread's messages for `files[]`
    // and bulk-load the bytes from `slack_attachments` + `cas_objects`.
    if let Some(cas_pool) = cas_pool.as_ref() {
        for bucket in &mut threads {
            let refs = collect_attachment_ref_ids(&bucket.messages);
            if refs.is_empty() {
                continue;
            }
            let ref_strs: Vec<&str> = refs.iter().map(String::as_str).collect();
            bucket.blobs =
                BlobBundle::load(&pool, cas_pool, ATTACHMENTS_PROJECTION_SQL, &ref_strs).await?;
        }
    }

    // A thread the diff named that no message still belongs to. The
    // bucket key is already the thread uuid render keys documents by, so
    // unlike every other provider there is no id to re-derive.
    let vanished_buckets = match scan.changed_threads.as_ref() {
        Some(changed) => {
            datalib_etl::doltlite_raw::buckets_without_rows(
                &pool,
                datalib_etl::pin::Reads::At(&pin),
                changed,
                &[("messages", "thread_root_uuid")],
            )
            .await?
        }
        None => Vec::new(),
    };

    Ok(ParsedSlack {
        workspace,
        users,
        channels,
        threads,
        docs_skipped,
        scan,
        vanished_buckets,
    })
}

/// Phase 1: union over the per-table dolt_diff vtabs to project
/// touched `thread_root_uuid`s. Workspace / users / channels changes
/// fan out to "render everything" — channel renames + user renames
/// appear inside every thread we render.
async fn scan_diff(
    pool: &SqlitePool,
    last_render_hash: Option<&str>,
    pin: &datalib_etl::pin::Pin,
) -> Result<ScanResult> {
    let scan = datalib_etl::doltlite_raw::scan_buckets(
        pool,
        last_render_hash,
        pin,
        &datalib_etl::doltlite_raw::DiffScanSpec {
            global_fanout_tables: &["workspaces", "users", "channels"],
            bucket_query: "
                SELECT DISTINCT thread_root_uuid FROM (
                    SELECT coalesce(to_thread_root_uuid, from_thread_root_uuid) AS thread_root_uuid
                      FROM dolt_diff_messages
                     WHERE from_ref = ?1 AND to_ref = ?2 AND diff_type != 'unchanged'
                    UNION
                    SELECT m.thread_root_uuid
                      FROM dolt_diff_slack_attachments d
                      JOIN pinned_messages m ON m.id = coalesce(d.to_message_uuid, d.from_message_uuid)
                     WHERE d.from_ref = ?1 AND d.to_ref = ?2 AND d.diff_type != 'unchanged'
                )
                WHERE thread_root_uuid IS NOT NULL
            ",
        },
    )
    .await?;
    Ok(ScanResult {
        changed_threads: scan.changed_buckets,
        new_head: scan.new_head,
        scan_elapsed: scan.scan_elapsed,
    })
}

async fn load_workspace(pool: &SqlitePool) -> Result<Option<Workspace>> {
    let row = sqlx::query(
        "SELECT json(payload) AS payload FROM pinned_workspaces workspaces ORDER BY id LIMIT 1",
    )
    .fetch_optional(pool)
    .await
    .context("select workspace")?;
    let Some(row) = row else { return Ok(None) };
    let Ok(s): Result<String, _> = row.try_get("payload") else {
        return Ok(None);
    };
    let Ok(v) = serde_json::from_str::<Value>(&s) else {
        return Ok(None);
    };
    let team_id = str_or(&v, "team_id");
    if team_id.is_empty() {
        return Ok(None);
    }
    Ok(Some(Workspace {
        team_id,
        team_name: opt_str(&v, "team"),
        team_url: opt_str(&v, "url"),
        self_user_id: opt_str(&v, "user_id"),
    }))
}

async fn load_users(pool: &SqlitePool) -> Result<BTreeMap<String, User>> {
    let rows = sqlx::query("SELECT id, team_id, json(payload) AS payload FROM pinned_users")
        .fetch_all(pool)
        .await
        .context("select users")?;
    let mut out: BTreeMap<String, User> = BTreeMap::new();
    for r in rows {
        let id: String = r.try_get("id").unwrap_or_default();
        if id.is_empty() {
            continue;
        }
        let team_id: String = r
            .try_get::<Option<String>, _>("team_id")
            .unwrap_or(None)
            .unwrap_or_default();
        let payload_str: String = match r.try_get("payload") {
            Ok(s) => s,
            Err(_) => continue,
        };
        let Ok(v) = serde_json::from_str::<Value>(&payload_str) else {
            continue;
        };
        let profile = v.get("profile");
        out.insert(
            id.clone(),
            User {
                user_id: id,
                team_id,
                name: opt_str(&v, "name"),
                real_name: opt_str(&v, "real_name")
                    .or_else(|| profile.and_then(|p| opt_str(p, "real_name"))),
                display_name: profile.and_then(|p| opt_str(p, "display_name")),
                email: profile.and_then(|p| opt_str(p, "email")),
            },
        );
    }
    Ok(out)
}

async fn load_channels(pool: &SqlitePool) -> Result<BTreeMap<String, Channel>> {
    // The DM columns landed after the first stores were written, and
    // this pool is read-only — it never runs `doltlite_raw::open`'s
    // schema reconcile, so a store the current downloader has not
    // touched still lacks them. Naming a missing column in the SELECT
    // fails at prepare time and sinks the render step, so probe first
    // and fall back to the columns that have always been there. Such a
    // store has no DMs in it anyway: they could not have been listed.
    let has_dm_columns = datalib_etl::doltlite_raw::column_exists(pool, "channels", "is_dm")
        .await?
        && datalib_etl::doltlite_raw::column_exists(pool, "channels", "dm_user_ids").await?;
    let sql = if has_dm_columns {
        "SELECT id, name, is_dm, dm_user_ids FROM pinned_channels"
    } else {
        "SELECT id, name FROM pinned_channels"
    };
    let rows = sqlx::query(sql)
        .fetch_all(pool)
        .await
        .context("select channels")?;
    let mut out: BTreeMap<String, Channel> = BTreeMap::new();
    for r in rows {
        let id: String = r.try_get("id").unwrap_or_default();
        if id.is_empty() {
            continue;
        }
        let name: Option<String> = r.try_get("name").ok().flatten();
        out.insert(
            id.clone(),
            Channel {
                channel_id: id,
                name,
                is_dm: r.try_get::<Option<i64>, _>("is_dm").ok().flatten() == Some(1),
                dm_user_ids: datalib_etl_slack::ingest::schema_raw::parse_dm_user_ids(
                    r.try_get::<Option<String>, _>("dm_user_ids")
                        .ok()
                        .flatten()
                        .as_deref(),
                ),
            },
        );
    }
    Ok(out)
}

/// Internal loaded-message shape carrying the thread_root_uuid column
/// (which `LoadedMessage` doesn't surface).
struct LoadedMessageWithThread {
    team_id: String,
    channel_id: String,
    ts: String,
    thread_ts: Option<String>,
    is_thread_root: bool,
    user_id: Option<String>,
    payload: Value,
    thread_root_uuid: String,
}

async fn thread_count(pool: &SqlitePool) -> Result<usize> {
    let row = sqlx::query_scalar::<_, i64>(
        "SELECT COUNT(DISTINCT thread_root_uuid) FROM pinned_messages messages WHERE payload IS NOT NULL",
    )
    .fetch_one(pool)
    .await
    .context("count threads")?;
    Ok(row as usize)
}

async fn load_all_messages(pool: &SqlitePool) -> Result<Vec<LoadedMessageWithThread>> {
    let rows = sqlx::query(
        "SELECT team_id, channel_id, ts, thread_ts, is_thread_root, user_id,
                json(payload) AS payload, thread_root_uuid
           FROM pinned_messages messages
          WHERE payload IS NOT NULL
          ORDER BY thread_root_uuid, ts",
    )
    .fetch_all(pool)
    .await
    .context("select all messages")?;
    Ok(rows_to_loaded(rows))
}

async fn load_messages_for_threads(
    pool: &SqlitePool,
    thread_uuids: &HashSet<String>,
) -> Result<Vec<LoadedMessageWithThread>> {
    if thread_uuids.is_empty() {
        return Ok(Vec::new());
    }
    const CHUNK: usize = 500;
    let uuids: Vec<&String> = thread_uuids.iter().collect();
    let mut out: Vec<LoadedMessageWithThread> = Vec::new();
    for chunk in uuids.chunks(CHUNK) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT team_id, channel_id, ts, thread_ts, is_thread_root, user_id,
                    json(payload) AS payload, thread_root_uuid
               FROM pinned_messages messages
              WHERE payload IS NOT NULL AND thread_root_uuid IN ({placeholders})
              ORDER BY thread_root_uuid, ts"
        );
        // Audited: static template; the only interpolation is a `?,?,?` run sized
        // from the chunk length. Every value is bound.
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for u in chunk {
            q = q.bind(u);
        }
        let rows = q
            .fetch_all(pool)
            .await
            .context("select messages for threads")?;
        out.extend(rows_to_loaded(rows));
    }
    Ok(out)
}

fn rows_to_loaded(rows: Vec<sqlx::sqlite::SqliteRow>) -> Vec<LoadedMessageWithThread> {
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
        out.push(LoadedMessageWithThread {
            team_id: r.try_get("team_id").unwrap_or_default(),
            channel_id: r.try_get("channel_id").unwrap_or_default(),
            ts: r.try_get("ts").unwrap_or_default(),
            thread_ts: r.try_get::<Option<String>, _>("thread_ts").unwrap_or(None),
            is_thread_root: is_root_int.unwrap_or(0) != 0,
            user_id: r.try_get::<Option<String>, _>("user_id").unwrap_or(None),
            payload,
            thread_root_uuid: r.try_get("thread_root_uuid").unwrap_or_default(),
        });
    }
    out
}

fn loaded_to_message(m: &LoadedMessageWithThread, default_team_id: &str) -> Message {
    let effective = m.thread_ts.clone().unwrap_or_else(|| m.ts.clone());
    Message {
        team_id: if m.team_id.is_empty() {
            default_team_id.to_string()
        } else {
            m.team_id.clone()
        },
        channel_id: m.channel_id.clone(),
        ts: m.ts.clone(),
        thread_ts: m.thread_ts.clone(),
        effective_thread_ts: effective,
        is_thread_root: m.is_thread_root,
        user_id: m.user_id.clone(),
        text: opt_str(&m.payload, "text").unwrap_or_default(),
        ts_iso: ts_to_iso(&m.ts),
        raw_json: m.payload.clone(),
    }
}

fn collect_attachment_ref_ids(msgs: &[Message]) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for m in msgs {
        let Some(files) = m.raw_json.get("files").and_then(|v| v.as_array()) else {
            continue;
        };
        for f in files {
            if let Some(id) = f.get("id").and_then(|v| v.as_str()) {
                if seen.insert(id.to_string()) {
                    out.push(id.to_string());
                }
            }
        }
    }
    out
}

// Legacy JSON-tree reader (kept for the in-crate TNG render fixture).

pub fn parse_raw_json_dir(out_dir: &Path) -> Result<ParsedSlack> {
    let raw_dir = out_dir.join("raw_api");
    let mut workspace: Option<Workspace> = None;
    let mut users: BTreeMap<String, User> = BTreeMap::new();
    let mut channels: BTreeMap<String, Channel> = BTreeMap::new();
    let mut messages_by_key: BTreeMap<(String, String), Message> = BTreeMap::new();

    for env in read_method_envelopes(&raw_dir, M_AUTH_TEST)? {
        let resp = env.get("response").cloned().unwrap_or(Value::Null);
        let team_id = str_or(&resp, "team_id");
        if team_id.is_empty() {
            continue;
        }
        workspace = Some(Workspace {
            team_id: team_id.clone(),
            team_name: opt_str(&resp, "team"),
            team_url: opt_str(&resp, "url"),
            self_user_id: opt_str(&resp, "user_id"),
        });
    }
    let team_id = workspace
        .as_ref()
        .map(|w| w.team_id.clone())
        .unwrap_or_else(|| "unknown".into());

    for env in read_method_envelopes(&raw_dir, M_USERS)? {
        let resp = env.get("response").cloned().unwrap_or(Value::Null);
        for u in array_field(&resp, "members") {
            ingest_user(u, &team_id, &mut users);
        }
    }
    for env in read_method_envelopes(&raw_dir, M_CHANNELS)? {
        let resp = env.get("response").cloned().unwrap_or(Value::Null);
        for c in array_field(&resp, "channels") {
            ingest_channel(c, &mut channels);
        }
    }
    for env in read_method_envelopes(&raw_dir, M_HISTORY)? {
        let params = env.get("params");
        let channel_id = params
            .and_then(|p| p.get("channel"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if channel_id.is_empty() {
            continue;
        }
        let resp = env.get("response").cloned().unwrap_or(Value::Null);
        for m in array_field(&resp, "messages") {
            let ts = str_or(m, "ts");
            if ts.is_empty() {
                continue;
            }
            let thread_ts = opt_str(m, "thread_ts");
            let effective = thread_ts.clone().unwrap_or_else(|| ts.clone());
            let is_root = match &thread_ts {
                None => true,
                Some(t_ts) => t_ts == &ts,
            };
            insert_message(
                &mut messages_by_key,
                &team_id,
                &channel_id,
                &ts,
                thread_ts,
                effective,
                is_root,
                m,
            );
        }
    }
    for env in read_method_envelopes(&raw_dir, M_REPLIES)? {
        let params = env.get("params");
        let channel_id = params
            .and_then(|p| p.get("channel"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let req_thread_ts = params
            .and_then(|p| p.get("ts"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        if channel_id.is_empty() || req_thread_ts.is_empty() {
            continue;
        }
        let resp = env.get("response").cloned().unwrap_or(Value::Null);
        for m in array_field(&resp, "messages") {
            let ts = str_or(m, "ts");
            if ts.is_empty() {
                continue;
            }
            let thread_ts = opt_str(m, "thread_ts").or_else(|| Some(req_thread_ts.clone()));
            let effective = thread_ts.clone().unwrap_or_else(|| ts.clone());
            let is_root = ts == req_thread_ts;
            insert_message(
                &mut messages_by_key,
                &team_id,
                &channel_id,
                &ts,
                thread_ts,
                effective,
                is_root,
                m,
            );
        }
    }

    // Bucket by thread.
    let mut by_thread: BTreeMap<String, Vec<Message>> = BTreeMap::new();
    for (_, msg) in messages_by_key {
        let uuid = slack_thread_uuid(&msg.team_id, &msg.channel_id, &msg.effective_thread_ts);
        by_thread.entry(uuid).or_default().push(msg);
    }
    let mut threads: Vec<SlackThreadBucket> = Vec::with_capacity(by_thread.len());
    for (thread_uuid, mut msgs) in by_thread {
        msgs.sort_by(|a, b| {
            (a.ts_iso.as_deref(), a.ts.as_str()).cmp(&(b.ts_iso.as_deref(), b.ts.as_str()))
        });
        threads.push(SlackThreadBucket {
            thread_uuid,
            messages: msgs,
            blobs: BlobBundle::default(),
        });
    }

    Ok(ParsedSlack {
        workspace,
        users,
        channels,
        threads,
        docs_skipped: 0,
        scan: ScanResult::default(),
        vanished_buckets: Vec::new(),
    })
}

fn ingest_user(u: &Value, default_team_id: &str, out: &mut BTreeMap<String, User>) {
    let id = str_or(u, "id");
    if id.is_empty() {
        return;
    }
    let profile = u.get("profile");
    out.insert(
        id.clone(),
        User {
            user_id: id,
            team_id: opt_str(u, "team_id").unwrap_or_else(|| default_team_id.to_string()),
            name: opt_str(u, "name"),
            real_name: opt_str(u, "real_name")
                .or_else(|| profile.and_then(|p| opt_str(p, "real_name"))),
            display_name: profile.and_then(|p| opt_str(p, "display_name")),
            email: profile.and_then(|p| opt_str(p, "email")),
        },
    );
}

fn ingest_channel(c: &Value, out: &mut BTreeMap<String, Channel>) {
    let id = str_or(c, "id");
    if id.is_empty() {
        return;
    }
    let flag = |k: &str| c.get(k).and_then(|v| v.as_bool()).unwrap_or(false);
    let is_im = flag("is_im");
    let is_dm = is_im || flag("is_mpim");
    out.insert(
        id.clone(),
        Channel {
            channel_id: id,
            name: opt_str(c, "name"),
            is_dm,
            dm_user_ids: if is_dm {
                datalib_etl_slack::ingest::db::dm_participants(c, is_im)
            } else {
                Vec::new()
            },
        },
    );
}

#[allow(clippy::too_many_arguments)]
fn insert_message(
    out: &mut BTreeMap<(String, String), Message>,
    team_id: &str,
    channel_id: &str,
    ts: &str,
    thread_ts: Option<String>,
    effective_thread_ts: String,
    is_thread_root: bool,
    raw: &Value,
) {
    let key = (channel_id.to_string(), ts.to_string());
    if out.contains_key(&key) {
        return;
    }
    let msg = Message {
        team_id: team_id.to_string(),
        channel_id: channel_id.to_string(),
        ts: ts.to_string(),
        thread_ts,
        effective_thread_ts,
        is_thread_root,
        user_id: opt_str(raw, "user"),
        text: opt_str(raw, "text").unwrap_or_default(),
        ts_iso: ts_to_iso(ts),
        raw_json: raw.clone(),
    };
    out.insert(key, msg);
}

fn read_method_envelopes(raw_dir: &Path, method: &str) -> Result<Vec<Value>> {
    let dir = raw_dir.join(method);
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .with_context(|| format!("read_dir {}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("jsonl"))
        .collect();
    files.sort();
    let mut out = Vec::new();
    for path in files {
        let f = File::open(&path).with_context(|| format!("open {}", path.display()))?;
        for line in BufReader::new(f).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let v: Value =
                serde_json::from_str(&line).with_context(|| format!("parse {}", path.display()))?;
            out.push(v);
        }
    }
    Ok(out)
}

fn str_or(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string()
}
fn opt_str(v: &Value, key: &str) -> Option<String> {
    v.get(key).and_then(|x| x.as_str()).map(str::to_string)
}
fn array_field<'a>(v: &'a Value, key: &str) -> &'a [Value] {
    v.get(key)
        .and_then(|x| x.as_array())
        .map(|a| a.as_slice())
        .unwrap_or(&[])
}

#[cfg(test)]
mod no_data_tests {
    use super::*;

    /// A source that has never been downloaded renders as empty, not
    /// as a failure: that is the normal state of every source in a
    /// freshly scaffolded config. See docs/dev/step_protocol.md,
    /// "Rendering a source with no data".
    #[test]
    fn parse_missing_source_returns_empty_silently() {
        let parsed = parse(Path::new("/this/does/not/exist"), None).unwrap();
        assert!(parsed.threads.is_empty());
        assert!(parsed.channels.is_empty());
        assert!(parsed.workspace.is_none());
    }
}

#[cfg(test)]
mod legacy_schema_tests {
    use super::*;
    // A deliberately *writable* pool: this test builds the legacy store it
    // then reads, so it cannot go through `open_reader`.
    use sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions};
    use std::str::FromStr;

    /// Render must survive a raw store written before the DM columns
    /// existed.
    #[tokio::test]
    async fn load_channels_tolerates_a_store_without_the_dm_columns() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("legacy.db");
        let opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", path.display()))
            .unwrap()
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(opts)
            .await
            .unwrap();
        // The `channels` shape as it was before this change.
        sqlx::query(
            "CREATE TABLE channels (id TEXT PRIMARY KEY, payload BLOB, name TEXT,
                                    is_member INTEGER, is_archived INTEGER)",
        )
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO channels (id, name, is_member) VALUES ('C1', 'bridge', 1)")
            .execute(&pool)
            .await
            .unwrap();

        // Read it the way render does: committed, pinned, through the views.
        // The store still lacks the DM columns, which is what this guards —
        // pinning does not conjure a column the store never had.
        datalib_etl::doltlite_raw::commit_run(&pool, "legacy store")
            .await
            .unwrap();
        let pin = datalib_etl::pin::head(&pool)
            .await
            .unwrap()
            .expect("the legacy store has a commit now");
        datalib_etl::pin::install_views(&pool, &pin).await.unwrap();

        let channels = load_channels(&pool)
            .await
            .expect("must not fail to prepare");
        let c = channels.get("C1").expect("C1");
        assert_eq!(c.name.as_deref(), Some("bridge"));
        // Such a store could never have listed a DM, so "not a DM" is
        // the truthful reading of the absent columns.
        assert!(!c.is_dm);
        assert!(c.dm_user_ids.is_empty());
        assert_eq!(c.display(&BTreeMap::new(), None), "#bridge");
    }
}
