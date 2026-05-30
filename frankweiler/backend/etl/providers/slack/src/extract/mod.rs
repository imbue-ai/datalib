//! Slack downloader entry point.
//!
//! Captures Slack data into a single doltlite db at
//! `<data_root>/raw/<name>.doltlite_db` — one row per workspace
//! (`auth.test`), user, channel, message, and reply page, plus the
//! shared `blobs` / `sync_runs` / `endpoint_shapes` tables. See `db.rs`
//! for the table layout and the rationale for keying messages by
//! `slack_message_uuid(team_id, channel_id, ts)`.
//!
//! Resume cursor: derived at startup from the DB.
//! `RawDb::latest_ts_by_channel` gives the per-channel `max(ts)` we've
//! ever recorded, and the next forward pass starts there. The trailing
//! refresh window re-queries the last N days; idempotent upserts
//! collapse no-op refresh passes to zero writes.

pub mod api;
pub mod db;
pub mod shapes;

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde_json::{json, Value};
use tracing::{info, info_span, instrument, warn, Instrument};

use api::{call_slack, SlackCall, SlackError};
pub use db::{
    block_on_load_all, block_on_load_filtered, block_on_probe_thread_cursors, db_path_for,
    BlobBytes, LoadedMessage, LoadedRaw, MessageRow, RawDb,
};
use frankweiler_etl::events;
use shapes::{M_AUTH_TEST, M_CHANNELS, M_HISTORY, M_REPLIES, M_USERS};

pub const DEFAULT_SINCE: &str = "2024-01-01";
pub const DEFAULT_REFRESH_WINDOW_DAYS: i64 = 30;

// ---------------------------------------------------------------------------
// Per-method drivers.
// ---------------------------------------------------------------------------

fn datetime_to_slack_ts(dt: &DateTime<Utc>) -> String {
    let secs = dt.timestamp();
    let nanos = dt.timestamp_subsec_micros();
    format!("{}.{:06}", secs, nanos)
}

fn empty_params() -> BTreeMap<String, String> {
    BTreeMap::new()
}

async fn call(method: &str, params: &BTreeMap<String, String>) -> Result<Value> {
    let SlackCall { response, .. } = call_slack(method, params)
        .await
        .map_err(|e: SlackError| anyhow::anyhow!("{}", e))?;
    Ok(response)
}

#[instrument(skip_all)]
async fn fetch_self(db: &RawDb) -> Result<String> {
    let resp = call(M_AUTH_TEST, &empty_params()).await?;
    db.upsert_workspace(&resp).await?;
    let team_id = resp
        .get("team_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("auth.test response missing team_id"))?
        .to_string();
    Ok(team_id)
}

/// Fetch every channel listing page, upserting each channel into the
/// DB along the way. Returns the set of channels visible to this run
/// (after the `members_only`/`include_archived` filter).
#[instrument(skip(db))]
async fn fetch_channels(
    db: &RawDb,
    members_only: bool,
    include_archived: bool,
) -> Result<Vec<(String, Option<String>)>> {
    let mut params = BTreeMap::new();
    params.insert(
        "exclude_archived".to_string(),
        if include_archived { "false" } else { "true" }.to_string(),
    );
    params.insert("limit".to_string(), "200".to_string());
    params.insert(
        "types".to_string(),
        "public_channel,private_channel".to_string(),
    );

    let mut cursor: Option<String> = None;
    loop {
        let mut p = params.clone();
        if let Some(c) = &cursor {
            p.insert("cursor".to_string(), c.clone());
        }
        let resp = call(M_CHANNELS, &p).await?;
        if let Some(arr) = resp.get("channels").and_then(|v| v.as_array()) {
            for ch in arr {
                db.upsert_channel(ch).await?;
            }
        }
        cursor = next_cursor(&resp);
        if cursor.is_none() || resp.get("has_more").and_then(|v| v.as_bool()) == Some(false) {
            break;
        }
    }
    db.channels_for_fetch(members_only, include_archived).await
}

#[instrument(skip_all)]
async fn fetch_users(db: &RawDb) -> Result<usize> {
    let mut base = BTreeMap::new();
    base.insert("limit".to_string(), "200".to_string());
    let t0 = std::time::Instant::now();
    let mut cursor: Option<String> = None;
    let mut count = 0usize;
    loop {
        let mut p = base.clone();
        if let Some(c) = &cursor {
            p.insert("cursor".to_string(), c.clone());
        }
        let resp = call(M_USERS, &p).await?;
        if let Some(arr) = resp.get("members").and_then(|v| v.as_array()) {
            for u in arr {
                db.upsert_user(u).await?;
            }
            count += arr.len();
        }
        cursor = next_cursor(&resp);
        if cursor.is_none() {
            break;
        }
    }
    events::indexed_batch("users", count, t0.elapsed().as_millis() as u64);
    Ok(count)
}

fn next_cursor(resp: &Value) -> Option<String> {
    resp.get("response_metadata")
        .and_then(|m| m.get("next_cursor"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Per-channel history + threads.
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn export_channel(
    db: &RawDb,
    team_id: &str,
    channel_id: &str,
    since_ts: &str,
    refresh_window_days: i64,
    channel_latest_ts: Option<&str>,
    latest_reply_by_thread: &std::collections::HashMap<(String, String), String>,
    now: &DateTime<Utc>,
    download_blobs: bool,
    totals: &mut ChannelTotals,
    progress: &frankweiler_etl::progress::Progress,
) -> Result<()> {
    // Pass A: list every history page, upsert top-level messages, and
    // download per-page media (preserves the existing commit-as-we-go
    // semantics for Ctrl-C safety). Thread replies are deferred so
    // we can announce a known total to the inner bar before starting
    // the long-tail fetch.
    let mut collected: Vec<Value> = Vec::new();

    let (forward_oldest, inclusive) = match channel_latest_ts {
        Some(ts) => (ts.to_string(), false),
        None => (since_ts.to_string(), true),
    };
    list_history(
        db,
        team_id,
        channel_id,
        &forward_oldest,
        inclusive,
        None,
        download_blobs,
        totals,
        progress,
        &mut collected,
    )
    .await?;

    if refresh_window_days > 0 {
        if let Some(latest_ts) = channel_latest_ts {
            let window_dt = *now - ChronoDuration::days(refresh_window_days);
            let window_oldest = datetime_to_slack_ts(&window_dt);
            if window_oldest.as_str() < latest_ts {
                let effective = if window_oldest.as_str() > since_ts {
                    window_oldest
                } else {
                    since_ts.to_string()
                };
                list_history(
                    db,
                    team_id,
                    channel_id,
                    &effective,
                    true,
                    Some(latest_ts),
                    download_blobs,
                    totals,
                    progress,
                    &mut collected,
                )
                .await?;
            }
        }
    }

    // Pass B: walk the collected top-level messages, fetching replies
    // for any thread whose latest_reply has advanced (or that we've
    // never seen). Now that we know how many replies are coming, the
    // inner bar can transition from spinner to determinate.
    let replies_to_fetch: u64 = collected
        .iter()
        .filter_map(|m| {
            let ts = m.get("ts").and_then(|v| v.as_str())?;
            let reply_count = m.get("reply_count").and_then(|v| v.as_i64()).unwrap_or(0);
            if reply_count <= 0 {
                return None;
            }
            let api_latest = m.get("latest_reply").and_then(|v| v.as_str());
            let stored = latest_reply_by_thread.get(&(channel_id.to_string(), ts.to_string()));
            if let (Some(api), Some(stored)) = (api_latest, stored.map(String::as_str)) {
                if stored >= api {
                    return None;
                }
            }
            Some(reply_count as u64)
        })
        .sum();
    progress.set_length(Some(totals.messages as u64 + replies_to_fetch));

    for m in &collected {
        let Some(ts) = m.get("ts").and_then(|v| v.as_str()) else {
            continue;
        };
        let reply_count = m.get("reply_count").and_then(|v| v.as_i64()).unwrap_or(0);
        if reply_count <= 0 {
            continue;
        }
        let api_latest = m.get("latest_reply").and_then(|v| v.as_str());
        let stored = latest_reply_by_thread.get(&(channel_id.to_string(), ts.to_string()));
        if let (Some(api), Some(stored)) = (api_latest, stored.map(String::as_str)) {
            if stored >= api {
                continue;
            }
        }
        let before = totals.replies;
        paginate_replies(db, team_id, channel_id, ts, download_blobs, totals).await?;
        let fetched = totals.replies.saturating_sub(before) as u64;
        progress.inc(fetched);
        let media_downloaded = totals.media.get("downloaded").copied().unwrap_or(0);
        progress.set_message(&format!(
            "msgs={} replies={} media={}",
            totals.messages, totals.replies, media_downloaded
        ));
    }

    Ok(())
}

#[derive(Default)]
struct ChannelTotals {
    messages: usize,
    replies: usize,
    media: BTreeMap<String, usize>,
}

/// Pass A of the per-channel export: walk `conversations.history`
/// page-by-page, upserting each top-level message and (per page)
/// downloading any media those messages reference. Threads are NOT
/// fetched here — the caller defers those to pass B so the inner
/// progress bar can announce a meaningful total before the long-tail
/// thread fetches begin. Every collected top-level message is appended
/// to `collected` for the caller to iterate in pass B.
#[allow(clippy::too_many_arguments)]
async fn list_history(
    db: &RawDb,
    team_id: &str,
    channel_id: &str,
    oldest_ts: &str,
    inclusive: bool,
    latest_ts: Option<&str>,
    download_blobs: bool,
    totals: &mut ChannelTotals,
    progress: &frankweiler_etl::progress::Progress,
    collected: &mut Vec<Value>,
) -> Result<()> {
    let mut base = BTreeMap::new();
    base.insert("channel".to_string(), channel_id.to_string());
    base.insert("oldest".to_string(), oldest_ts.to_string());
    base.insert(
        "inclusive".to_string(),
        if inclusive { "true" } else { "false" }.to_string(),
    );
    base.insert("include_all_metadata".to_string(), "true".to_string());
    base.insert("limit".to_string(), "200".to_string());
    if let Some(l) = latest_ts {
        base.insert("latest".to_string(), l.to_string());
    }

    let mut cursor: Option<String> = None;
    loop {
        let mut params = base.clone();
        if let Some(c) = &cursor {
            params.insert("cursor".to_string(), c.clone());
        }
        let resp = call(M_HISTORY, &params).await?;
        let messages: Vec<Value> = resp
            .get("messages")
            .and_then(|v| v.as_array())
            .map(|a| a.to_vec())
            .unwrap_or_default();

        for m in &messages {
            upsert_history_message(db, team_id, channel_id, m).await?;
            totals.messages += 1;
            progress.inc(1);
        }

        // Per-page media. Sequential — Slack hates concurrency on
        // files.slack.com. Threads (in pass B) download their replies'
        // files inline inside `paginate_replies`.
        if download_blobs {
            let counts = api::download_files_for_messages(db, channel_id, &messages).await?;
            for (k, v) in counts {
                *totals.media.entry(k).or_insert(0) += v;
            }
        }

        let media_downloaded = totals.media.get("downloaded").copied().unwrap_or(0);
        progress.set_message(&format!(
            "listing  msgs={} media={}",
            totals.messages, media_downloaded
        ));

        collected.extend(messages);

        cursor = next_cursor(&resp);
        if cursor.is_none() || resp.get("has_more").and_then(|v| v.as_bool()) == Some(false) {
            break;
        }
    }
    Ok(())
}

/// Paginate `conversations.replies` for one thread. Upserts every
/// message in the response (including the parent re-served by Slack)
/// and records a `replies_pages` row so the next sync can skip if no
/// new replies have landed.
async fn paginate_replies(
    db: &RawDb,
    team_id: &str,
    channel_id: &str,
    thread_ts: &str,
    download_blobs: bool,
    totals: &mut ChannelTotals,
) -> Result<()> {
    let mut base = BTreeMap::new();
    base.insert("channel".to_string(), channel_id.to_string());
    base.insert("ts".to_string(), thread_ts.to_string());
    base.insert("limit".to_string(), "200".to_string());

    let mut cursor: Option<String> = None;
    let mut last_seen_reply: Option<String> = None;
    loop {
        let mut p = base.clone();
        if let Some(c) = &cursor {
            p.insert("cursor".to_string(), c.clone());
        }
        let resp = call(M_REPLIES, &p).await?;
        let msgs: Vec<Value> = resp
            .get("messages")
            .and_then(|v| v.as_array())
            .map(|a| a.to_vec())
            .unwrap_or_default();

        for m in &msgs {
            upsert_reply_message(db, team_id, channel_id, thread_ts, m).await?;
            if let Some(ts) = m.get("ts").and_then(|v| v.as_str()) {
                if ts != thread_ts {
                    // Track the max child ts we've seen so the
                    // replies_pages bookmark advances monotonically.
                    if last_seen_reply.as_deref().is_none_or(|prev| ts > prev) {
                        last_seen_reply = Some(ts.to_string());
                    }
                }
            }
        }
        // Total reply-children is messages minus the parent (one per
        // page only carries the parent at most once).
        totals.replies += msgs.len().saturating_sub(1);

        if download_blobs {
            let counts = api::download_files_for_messages(db, channel_id, &msgs).await?;
            for (k, v) in counts {
                *totals.media.entry(k).or_insert(0) += v;
            }
        }
        cursor = next_cursor(&resp);
        if cursor.is_none() || resp.get("has_more").and_then(|v| v.as_bool()) == Some(false) {
            break;
        }
    }
    db.upsert_replies_page(channel_id, thread_ts, last_seen_reply.as_deref())
        .await?;
    Ok(())
}

async fn upsert_history_message(
    db: &RawDb,
    team_id: &str,
    channel_id: &str,
    m: &Value,
) -> Result<()> {
    let Some(ts) = m.get("ts").and_then(|v| v.as_str()) else {
        return Ok(());
    };
    let thread_ts = m
        .get("thread_ts")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let is_thread_root = match thread_ts.as_deref() {
        None => true,
        Some(tts) => tts == ts,
    };
    let row = db::MessageRow {
        team_id: team_id.to_string(),
        channel_id: channel_id.to_string(),
        ts: ts.to_string(),
        thread_ts,
        is_thread_root,
        user_id: m.get("user").and_then(|v| v.as_str()).map(String::from),
        payload: m.clone(),
    };
    db.upsert_message(&row).await
}

async fn upsert_reply_message(
    db: &RawDb,
    team_id: &str,
    channel_id: &str,
    requested_thread_ts: &str,
    m: &Value,
) -> Result<()> {
    let Some(ts) = m.get("ts").and_then(|v| v.as_str()) else {
        return Ok(());
    };
    // Slack returns the parent inline with replies; treat ts == requested
    // as the root regardless of which endpoint delivered it. Replies
    // that omit `thread_ts` get it filled in from the request.
    let thread_ts = m
        .get("thread_ts")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| Some(requested_thread_ts.to_string()));
    let is_thread_root = ts == requested_thread_ts;
    let row = db::MessageRow {
        team_id: team_id.to_string(),
        channel_id: channel_id.to_string(),
        ts: ts.to_string(),
        thread_ts,
        is_thread_root,
        user_id: m.get("user").and_then(|v| v.as_str()).map(String::from),
        payload: m.clone(),
    };
    db.upsert_message(&row).await
}

// ---------------------------------------------------------------------------
// Public entry point.
// ---------------------------------------------------------------------------

pub struct FetchOptions {
    /// Path to the doltlite database file. If the caller passes a
    /// legacy directory, it's rewritten to `<dir>.doltlite_db`.
    pub db_path: PathBuf,
    pub channels: Option<Vec<String>>,
    pub since: String,
    pub refresh_window_days: i64,
    pub members_only: bool,
    pub media: bool,
    pub progress: frankweiler_etl::progress::Progress,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            db_path: PathBuf::new(),
            channels: None,
            since: DEFAULT_SINCE.to_string(),
            refresh_window_days: DEFAULT_REFRESH_WINDOW_DAYS,
            members_only: true,
            media: true,
            progress: frankweiler_etl::progress::Progress::noop(),
        }
    }
}

pub struct FetchSummary {
    pub messages: usize,
    pub replies: usize,
    pub media: BTreeMap<String, usize>,
}

#[instrument(skip_all, fields(db = %opts.db_path.display()))]
pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db_path = db_path_for(&opts.db_path);
    let _ = frankweiler_etl::latchkey::ensure_curl_shim();
    let db = RawDb::open(&db_path)
        .await
        .with_context(|| format!("open raw db {}", db_path.display()))?;

    let since_dt =
        parse_iso_or_utc_date(&opts.since).with_context(|| format!("--since {:?}", opts.since))?;
    let since_ts = datetime_to_slack_ts(&since_dt);
    let now = Utc::now();

    let run_config = json!({
        "channels": opts.channels,
        "since": opts.since,
        "refresh_window_days": opts.refresh_window_days,
        "members_only": opts.members_only,
        "media": opts.media,
    });
    let run_id = db.start_run(&run_config).await?;

    let channel_latest_ts = db.latest_ts_by_channel().await?;
    let latest_reply_map = db.latest_reply_by_thread().await?;

    let mut grand = FetchSummary {
        messages: 0,
        replies: 0,
        media: BTreeMap::new(),
    };

    let work = async {
        let team_id = fetch_self(&db).await?;
        let visible_channels =
            fetch_channels(&db, opts.members_only, opts.channels.is_some()).await?;
        fetch_users(&db).await?;

        let targets: Vec<(String, String)> = match &opts.channels {
            Some(names) => {
                let by_name: BTreeMap<String, String> = visible_channels
                    .iter()
                    .filter_map(|(id, name)| name.as_ref().map(|n| (n.clone(), id.clone())))
                    .collect();
                names
                    .iter()
                    .filter_map(|spec| {
                        let name = spec.trim_start_matches('#').to_string();
                        by_name.get(&name).map(|id| (id.clone(), name))
                    })
                    .collect()
            }
            None => visible_channels
                .iter()
                .map(|(id, name)| (id.clone(), name.clone().unwrap_or_else(|| id.clone())))
                .collect(),
        };
        info!(
            event = "slack_export_planned",
            channels = targets.len(),
            media = opts.media,
        );

        opts.progress.set_length(Some(targets.len() as u64));
        for (cid, name) in &targets {
            opts.progress.set_message(name);
            let span = info_span!("channel", channel_name = %name, channel_id = %cid);
            let mut totals = ChannelTotals::default();
            // Per-channel inner bar: starts as a spinner during the
            // list pass (total unknown) and switches to a determinate
            // bar in pass B once `export_channel` calls `set_length`.
            let inner = opts.progress.child(name);
            inner.set_message("listing");
            let result = export_channel(
                &db,
                &team_id,
                cid,
                &since_ts,
                opts.refresh_window_days,
                channel_latest_ts.get(cid).map(|s| s.as_str()),
                &latest_reply_map,
                &now,
                opts.media,
                &mut totals,
                &inner,
            )
            .instrument(span)
            .await;
            inner.finish(&format!(
                "done msgs={} replies={} media={}",
                totals.messages,
                totals.replies,
                totals.media.get("downloaded").copied().unwrap_or(0),
            ));
            opts.progress.inc(1);
            match result {
                Ok(()) => {
                    grand.messages += totals.messages;
                    grand.replies += totals.replies;
                    for (k, v) in totals.media {
                        *grand.media.entry(k).or_insert(0) += v;
                    }
                }
                Err(e) => warn!(event = "slack_channel_failed", channel = %name, error = %e),
            }
        }
        Ok::<(), anyhow::Error>(())
    };

    let result = work.await;
    let summary_json = json!({
        "messages": grand.messages,
        "replies": grand.replies,
        "media": grand.media,
        "error": result.as_ref().err().map(|e| e.to_string()),
    });
    let status = if result.is_ok() { "ok" } else { "error" };
    let _ = db.finish_run(run_id, status, &summary_json).await;
    result?;

    info!(
        event = "slack_export_complete",
        messages = grand.messages,
        replies = grand.replies,
    );
    Ok(grand)
}

fn parse_iso_or_utc_date(s: &str) -> Result<DateTime<Utc>> {
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Ok(d.with_timezone(&Utc));
    }
    let naive = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").context("expected ISO date")?;
    let ndt = naive
        .and_hms_opt(0, 0, 0)
        .context("invalid date components")?;
    Ok(DateTime::<Utc>::from_naive_utc_and_offset(ndt, Utc))
}
