//! Slack downloader entry point.
//!
//! Captures raw Slack API responses verbatim into
//! `<out>/raw_api/<method>/events.jsonl`. Each page of each method gets
//! one envelope record `{_recorded_at, method, params, duration_ms,
//! response}`. Pages whose every item is a content-match for prior
//! captures are skipped, so the on-disk size tracks unique-content
//! growth rather than poll frequency.
//!
//! Deriving "the latest set of users", "messages in channel X", etc.
//! from this stream is the translator's job — this layer is concerned
//! only with faithful capture + dedup.
//!
//! Resume cursor: derived at startup from the dedup index. For each
//! channel we know the max `ts` we've ever recorded, and the next
//! forward pass starts there. The trailing refresh window re-queries
//! the last N days; the dedup layer collapses no-op refresh passes to
//! zero writes.

pub mod api;
pub mod shapes;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde_json::Value;
use tracing::{info, info_span, instrument, warn, Instrument};

use api::{call_slack, SlackCall, SlackError};
use frankweiler_etl::obs::events;
use frankweiler_etl::raw_store::{PageCapture, RawStore};
use shapes::{
    items_in_response, latest_reply_by_thread, latest_ts_by_channel, M_AUTH_TEST, M_CHANNELS,
    M_HISTORY, M_REPLIES, M_USERS,
};

pub const DEFAULT_SINCE: &str = "2024-01-01";
pub const DEFAULT_REFRESH_WINDOW_DAYS: i64 = 30;

// ---------------------------------------------------------------------------
// Page capture helper. Wraps `call_slack` + `RawStore::save_page` so each
// API call site is one statement.
// ---------------------------------------------------------------------------

async fn call_and_save(
    store: &mut RawStore,
    method: &str,
    params: &BTreeMap<String, String>,
) -> Result<Value> {
    let SlackCall {
        response,
        duration_ms,
    } = call_slack(method, params)
        .await
        .map_err(|e: SlackError| anyhow::anyhow!("{}", e))?;
    let items = items_in_response(method, params, &response);
    let cap = PageCapture {
        method,
        params,
        duration_ms,
        response: response.clone(),
        items,
    };
    store.save_page(cap)?;
    Ok(response)
}

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

#[instrument(skip_all)]
async fn fetch_self(store: &mut RawStore) -> Result<()> {
    call_and_save(store, M_AUTH_TEST, &empty_params()).await?;
    Ok(())
}

/// Fetch every channel page; `members_only`/`include_archived` filter
/// what we *return* to the caller for fan-out, but we capture every page
/// verbatim regardless.
#[instrument(skip(store))]
async fn fetch_channels(
    store: &mut RawStore,
    members_only: bool,
    include_archived: bool,
) -> Result<Vec<Value>> {
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

    let mut all: Vec<Value> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let mut p = params.clone();
        if let Some(c) = &cursor {
            p.insert("cursor".to_string(), c.clone());
        }
        let resp = call_and_save(store, M_CHANNELS, &p).await?;
        if let Some(arr) = resp.get("channels").and_then(|v| v.as_array()) {
            all.extend(arr.iter().cloned());
        }
        cursor = next_cursor(&resp);
        if cursor.is_none() || resp.get("has_more").and_then(|v| v.as_bool()) == Some(false) {
            break;
        }
    }
    if members_only {
        all.retain(|c| {
            c.get("is_member")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        });
    }
    Ok(all)
}

#[instrument(skip_all)]
async fn fetch_users(store: &mut RawStore) -> Result<()> {
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
        let resp = call_and_save(store, M_USERS, &p).await?;
        if let Some(arr) = resp.get("members").and_then(|v| v.as_array()) {
            count += arr.len();
        }
        cursor = next_cursor(&resp);
        if cursor.is_none() {
            break;
        }
    }
    events::indexed_batch("users", count, t0.elapsed().as_millis() as u64);
    Ok(())
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
    store: &mut RawStore,
    channel_id: &str,
    since_ts: &str,
    refresh_window_days: i64,
    channel_latest_ts: Option<&str>,
    latest_reply_by_thread: &BTreeMap<(String, String), String>,
    now: &DateTime<Utc>,
    media_dir: Option<&Path>,
    totals: &mut ChannelTotals,
    progress: &frankweiler_etl::progress::Progress,
) -> Result<()> {
    let (forward_oldest, inclusive) = match channel_latest_ts {
        Some(ts) => (ts.to_string(), false),
        None => (since_ts.to_string(), true),
    };
    paginate_history(
        store,
        channel_id,
        &forward_oldest,
        inclusive,
        None,
        latest_reply_by_thread,
        media_dir,
        totals,
        progress,
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
                paginate_history(
                    store,
                    channel_id,
                    &effective,
                    true,
                    Some(latest_ts),
                    latest_reply_by_thread,
                    media_dir,
                    totals,
                    progress,
                )
                .await?;
            }
        }
    }
    Ok(())
}

#[derive(Default)]
struct ChannelTotals {
    messages: usize,
    replies: usize,
    media: BTreeMap<String, usize>,
}

/// Walk `conversations.history` page-by-page, saving each page before
/// requesting the next so a Ctrl-C loses at most one in-flight page.
#[allow(clippy::too_many_arguments)]
async fn paginate_history(
    store: &mut RawStore,
    channel_id: &str,
    oldest_ts: &str,
    inclusive: bool,
    latest_ts: Option<&str>,
    latest_reply_by_thread: &BTreeMap<(String, String), String>,
    media_dir: Option<&Path>,
    totals: &mut ChannelTotals,
    progress: &frankweiler_etl::progress::Progress,
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
        let resp = call_and_save(store, M_HISTORY, &params).await?;
        let messages: Vec<Value> = resp
            .get("messages")
            .and_then(|v| v.as_array())
            .map(|a| a.to_vec())
            .unwrap_or_default();

        totals.messages += messages.len();

        // Threads → conversations.replies. Skip threads whose latest
        // reply we already have on disk.
        for m in &messages {
            let ts = match m.get("ts").and_then(|v| v.as_str()) {
                Some(s) => s,
                None => continue,
            };
            let reply_count = m.get("reply_count").and_then(|v| v.as_i64()).unwrap_or(0);
            if reply_count == 0 {
                continue;
            }
            let api_latest = m.get("latest_reply").and_then(|v| v.as_str());
            let stored = latest_reply_by_thread.get(&(channel_id.to_string(), ts.to_string()));
            if let (Some(api), Some(stored)) = (api_latest, stored.map(String::as_str)) {
                if stored >= api {
                    continue;
                }
            }
            paginate_replies(store, channel_id, ts, media_dir, totals).await?;
        }

        // Media for this page (sequential — Slack hates concurrency on
        // files.slack.com). Replies fetched above will trigger their own
        // file downloads via the call_and_save → response path? No — we
        // didn't keep the replies responses around here. Walk both
        // messages and any replies we fetched: simpler to fold replies
        // into media download from inside paginate_replies.
        if let Some(md) = media_dir {
            let counts = api::download_files_for_messages(&messages, md).await?;
            for (k, v) in counts {
                *totals.media.entry(k).or_insert(0) += v;
            }
        }

        // Push cumulative counters through the unified Progress sink.
        // Both the indicatif bar and any structured (tracing) sink
        // receive them from the same emission point.
        let media_downloaded = totals.media.get("downloaded").copied().unwrap_or(0);
        progress.set_message(&format!(
            "{channel_id} msgs={} replies={} media={}",
            totals.messages, totals.replies, media_downloaded
        ));

        cursor = next_cursor(&resp);
        if cursor.is_none() || resp.get("has_more").and_then(|v| v.as_bool()) == Some(false) {
            break;
        }
    }
    Ok(())
}

/// Paginate `conversations.replies` for one thread, saving each page.
/// Returns the count of reply messages seen this call. Media downloads
/// happen inline so file uploads in threaded replies aren't missed.
async fn paginate_replies(
    store: &mut RawStore,
    channel_id: &str,
    thread_ts: &str,
    media_dir: Option<&Path>,
    totals: &mut ChannelTotals,
) -> Result<()> {
    let mut base = BTreeMap::new();
    base.insert("channel".to_string(), channel_id.to_string());
    base.insert("ts".to_string(), thread_ts.to_string());
    base.insert("limit".to_string(), "200".to_string());

    let mut cursor: Option<String> = None;
    loop {
        let mut p = base.clone();
        if let Some(c) = &cursor {
            p.insert("cursor".to_string(), c.clone());
        }
        let resp = call_and_save(store, M_REPLIES, &p).await?;
        let msgs: Vec<Value> = resp
            .get("messages")
            .and_then(|v| v.as_array())
            .map(|a| a.to_vec())
            .unwrap_or_default();
        totals.replies += msgs.len().saturating_sub(1); // exclude the parent
        if let Some(md) = media_dir {
            let counts = api::download_files_for_messages(&msgs, md).await?;
            for (k, v) in counts {
                *totals.media.entry(k).or_insert(0) += v;
            }
        }
        cursor = next_cursor(&resp);
        if cursor.is_none() || resp.get("has_more").and_then(|v| v.as_bool()) == Some(false) {
            break;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Public entry point.
// ---------------------------------------------------------------------------

pub struct FetchOptions {
    pub out_dir: PathBuf,
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
            out_dir: PathBuf::new(),
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

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    std::fs::create_dir_all(&opts.out_dir)
        .with_context(|| format!("mkdir -p {}", opts.out_dir.display()))?;

    let since_dt =
        parse_iso_or_utc_date(&opts.since).with_context(|| format!("--since {:?}", opts.since))?;
    let since_ts = datetime_to_slack_ts(&since_dt);
    let now = Utc::now();

    let mut store = RawStore::load(&opts.out_dir)?;

    let channel_latest_ts = latest_ts_by_channel(store.keys_for(M_HISTORY));
    let latest_reply_map = latest_reply_by_thread(store.keys_for(M_REPLIES));

    info!(
        event = "slack_state_loaded",
        channels_indexed = store.seen_count(M_CHANNELS),
        users_indexed = store.seen_count(M_USERS),
        messages_indexed = store.seen_count(M_HISTORY),
        replies_indexed = store.seen_count(M_REPLIES),
    );

    fetch_self(&mut store).await?;
    let fresh_channels =
        fetch_channels(&mut store, opts.members_only, opts.channels.is_some()).await?;

    let mut name_to_id: BTreeMap<String, String> = BTreeMap::new();
    for c in &fresh_channels {
        if let (Some(n), Some(i)) = (
            c.get("name").and_then(|v| v.as_str()),
            c.get("id").and_then(|v| v.as_str()),
        ) {
            name_to_id.insert(n.to_string(), i.to_string());
        }
    }

    fetch_users(&mut store).await?;

    let targets: Vec<(String, String)> = match &opts.channels {
        Some(names) => names
            .iter()
            .filter_map(|spec| {
                let name = spec.trim_start_matches('#');
                name_to_id
                    .get(name)
                    .map(|id| (id.clone(), name.to_string()))
            })
            .collect(),
        None => fresh_channels
            .iter()
            .filter_map(|c| {
                let n = c.get("name").and_then(|v| v.as_str())?;
                let i = c.get("id").and_then(|v| v.as_str())?;
                Some((i.to_string(), n.to_string()))
            })
            .collect(),
    };

    let media_dir: Option<PathBuf> = if opts.media {
        Some(opts.out_dir.join(frankweiler_etl::media::BLOBS_DIR))
    } else {
        None
    };
    info!(
        event = "slack_export_planned",
        channels = targets.len(),
        media = opts.media,
    );

    let mut grand = FetchSummary {
        messages: 0,
        replies: 0,
        media: BTreeMap::new(),
    };
    opts.progress.set_length(Some(targets.len() as u64));
    for (cid, name) in &targets {
        opts.progress.inc(1);
        opts.progress.set_message(name);
        let span = info_span!("channel", channel_name = %name, channel_id = %cid);
        let mut totals = ChannelTotals::default();
        let result = export_channel(
            &mut store,
            cid,
            &since_ts,
            opts.refresh_window_days,
            channel_latest_ts.get(cid).map(|s| s.as_str()),
            &latest_reply_map,
            &now,
            media_dir.as_deref(),
            &mut totals,
            &opts.progress,
        )
        .instrument(span)
        .await;
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
