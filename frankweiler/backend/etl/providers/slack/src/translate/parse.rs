//! Doltlite-aware parse entry point. Two-phase:
//!
//!   1. `dolt_diff_<table>` scan — ask doltlite which thread roots
//!      changed since the last successful render.
//!   2. Filtered load — pull only changed threads' messages out of
//!      the DB, then load each thread's per-bucket [`BlobBundle`]
//!      from `slack_attachments` + `cas_objects` in two SQL queries.
//!
//! Cold start (`last_render_hash = None`) loads every thread. Same
//! path is taken when the JSON-tree fallback fires (in-crate render
//! fixture).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::Duration;

use anyhow::{Context, Result};
use frankweiler_etl::blob_cas::{self, BlobBundle};
use serde_json::Value;
use sqlx::sqlite::{SqliteConnectOptions, SqlitePool, SqlitePoolOptions};
use sqlx::Row;

use crate::extract::db::db_path_for;
use crate::extract::schema_raw::slack_thread_uuid;
use crate::extract::shapes::{M_AUTH_TEST, M_CHANNELS, M_HISTORY, M_REPLIES, M_USERS};

use super::{ts_to_iso, Channel, Message, User, Workspace};

/// SQL projection that maps a Slack `file_id` to its CAS blake3.
/// Used by [`BlobBundle::load`] from the per-thread load below.
const ATTACHMENTS_PROJECTION_SQL: &str = "
    SELECT file_id AS ref_id, MAX(blake3) AS blake3,
           NULL AS content_type, NULL AS upstream_name
      FROM slack_attachments
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
}

impl ParsedSlack {
    pub fn fallback_team_id(&self) -> &str {
        self.workspace
            .as_ref()
            .map(|w| w.team_id.as_str())
            .unwrap_or("unknown")
    }
}

/// Two-phase parse. Cold start (`last_render_hash = None`) renders
/// every thread; same path when `path` resolves to a legacy JSON tree.
pub fn parse(path: &Path, last_render_hash: Option<&str>) -> Result<ParsedSlack> {
    let db_path = db_path_for(path);
    if db_path.exists() {
        return parse_doltlite(&db_path, last_render_hash);
    }
    if path.is_dir() {
        return parse_raw_json_dir(path);
    }
    anyhow::bail!(
        "slack source not found at {} (no .doltlite_db, no JSONL tree)",
        path.display()
    )
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
    let opts =
        SqliteConnectOptions::from_str(&format!("sqlite://{}", db_path.display()))?.read_only(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(60))
        .connect_with(opts)
        .await
        .with_context(|| format!("open slack doltlite for translate {}", db_path.display()))?;

    let cas_path = blob_cas::cas_path_for(db_path);
    let cas_pool: Option<SqlitePool> = if cas_path.is_file() {
        let cas_opts = SqliteConnectOptions::from_str(&format!("sqlite://{}", cas_path.display()))?
            .read_only(true);
        Some(
            SqlitePoolOptions::new()
                .max_connections(1)
                .acquire_timeout(Duration::from_secs(60))
                .connect_with(cas_opts)
                .await
                .with_context(|| format!("open slack CAS for translate {}", cas_path.display()))?,
        )
    } else {
        None
    };

    let scan = scan_diff(&pool, last_render_hash).await?;

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
            (a.ts_iso.as_str(), a.ts.as_str()).cmp(&(b.ts_iso.as_str(), b.ts.as_str()))
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

    Ok(ParsedSlack {
        workspace,
        users,
        channels,
        threads,
        docs_skipped,
        scan,
    })
}

/// Phase 1: union over the per-table dolt_diff vtabs to project
/// touched `thread_root_uuid`s. Workspace / users / channels changes
/// fan out to "render everything" — channel renames + user renames
/// appear inside every thread we render.
async fn scan_diff(pool: &SqlitePool, last_render_hash: Option<&str>) -> Result<ScanResult> {
    let new_head: Option<String> =
        sqlx::query_scalar("SELECT commit_hash FROM dolt_log() ORDER BY date DESC LIMIT 1")
            .fetch_optional(pool)
            .await
            .ok()
            .flatten();

    let Some(from_ref) = last_render_hash else {
        return Ok(ScanResult {
            changed_threads: None,
            new_head,
            scan_elapsed: None,
        });
    };

    // Workspace / users / channels are referenced by every thread's
    // render. Any change → repaint everything.
    let any_global: Option<i64> = sqlx::query_scalar(
        "SELECT 1 FROM dolt_diff_workspaces \
          WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged' \
         UNION ALL \
         SELECT 1 FROM dolt_diff_users \
          WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged' \
         UNION ALL \
         SELECT 1 FROM dolt_diff_channels \
          WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged' \
         LIMIT 1",
    )
    .bind(from_ref)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    if any_global.is_some() {
        return Ok(ScanResult {
            changed_threads: None,
            new_head,
            scan_elapsed: None,
        });
    }

    let sql = "
        SELECT DISTINCT thread_root_uuid FROM (
            SELECT coalesce(to_thread_root_uuid, from_thread_root_uuid) AS thread_root_uuid
              FROM dolt_diff_messages
             WHERE from_ref = ?1 AND to_ref = 'HEAD' AND diff_type != 'unchanged'
            UNION
            SELECT m.thread_root_uuid
              FROM dolt_diff_slack_attachments d
              JOIN messages m ON m.id = coalesce(d.to_message_uuid, d.from_message_uuid)
             WHERE d.from_ref = ?1 AND d.to_ref = 'HEAD' AND d.diff_type != 'unchanged'
        )
        WHERE thread_root_uuid IS NOT NULL
    ";
    let started = std::time::Instant::now();
    let res = sqlx::query(sql).bind(from_ref).fetch_all(pool).await;
    let elapsed = started.elapsed();
    let rows = match res {
        Ok(rows) => rows,
        Err(e) => {
            tracing::info!(
                source = "slack",
                error = %e,
                "dolt_diff scan failed — falling back to cold-start (render everything)"
            );
            return Ok(ScanResult {
                changed_threads: None,
                new_head,
                scan_elapsed: Some(elapsed),
            });
        }
    };
    let set: HashSet<String> = rows.iter().map(|r| r.get::<String, _>(0)).collect();
    Ok(ScanResult {
        changed_threads: Some(set),
        new_head,
        scan_elapsed: Some(elapsed),
    })
}

async fn load_workspace(pool: &SqlitePool) -> Result<Option<Workspace>> {
    let row = sqlx::query("SELECT json(payload) AS payload FROM workspaces ORDER BY id LIMIT 1")
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
    let rows = sqlx::query("SELECT id, team_id, json(payload) AS payload FROM users")
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
            },
        );
    }
    Ok(out)
}

async fn load_channels(pool: &SqlitePool) -> Result<BTreeMap<String, Channel>> {
    let rows = sqlx::query("SELECT id, name FROM channels")
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
        "SELECT COUNT(DISTINCT thread_root_uuid) FROM messages WHERE payload IS NOT NULL",
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
           FROM messages
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

/// Walk all messages in a thread to enumerate the attachment file_ids
/// it references. Same shape as render's `files()` extraction.
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

// ---------------------------------------------------------------------------
// Legacy JSON-tree reader (kept for the in-crate TNG render fixture).
// ---------------------------------------------------------------------------

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
            (a.ts_iso.as_str(), a.ts.as_str()).cmp(&(b.ts_iso.as_str(), b.ts.as_str()))
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
        },
    );
}

fn ingest_channel(c: &Value, out: &mut BTreeMap<String, Channel>) {
    let id = str_or(c, "id");
    if id.is_empty() {
        return;
    }
    out.insert(
        id.clone(),
        Channel {
            channel_id: id,
            name: opt_str(c, "name"),
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

#[allow(dead_code)] // exists so the dead-code linter doesn't flag the import
fn _dummy_use() -> HashMap<(), ()> {
    HashMap::new()
}
