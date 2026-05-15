//! Slack downloader entry point. Mirrors `src/download/slack_web.py`:
//! per-entity event streams (self_identity / channel / user / message /
//! reply / reaction), each split into created/ + updated/ JSONL files.
//!
//! Raw API responses are stored verbatim under `raw` — no field dropping,
//! since we want to keep full fidelity for later schema evolution.

pub mod api;

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde_json::{json, Value};

use crate::event_store::{diff_and_save, load_latest_by_key, make_record};
use api::{call_slack, paginate, SlackError};

pub const DEFAULT_SINCE: &str = "2024-01-01";
pub const DEFAULT_REFRESH_WINDOW_DAYS: i64 = 30;

const ENTITY_CHANNEL: &str = "channel";
const ENTITY_USER: &str = "user";
const ENTITY_MESSAGE: &str = "message";
const ENTITY_REPLY: &str = "reply";
const ENTITY_REACTION: &str = "reaction";
const ENTITY_SELF: &str = "self_identity";

// ---------------------------------------------------------------------------
// Key extraction. Each is rendered as a tab-joined string for BTreeMap use.
// ---------------------------------------------------------------------------

fn k_channel(rec: &Value) -> String {
    rec.get("channel_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn k_user(rec: &Value) -> String {
    rec.get("user_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

fn k_message(rec: &Value) -> String {
    let c = rec.get("channel_id").and_then(|v| v.as_str()).unwrap_or("");
    let t = rec.get("message_ts").and_then(|v| v.as_str()).unwrap_or("");
    format!("{}\t{}", c, t)
}

fn k_reply(rec: &Value) -> String {
    let c = rec.get("channel_id").and_then(|v| v.as_str()).unwrap_or("");
    let t = rec.get("thread_ts").and_then(|v| v.as_str()).unwrap_or("");
    let r = rec.get("reply_ts").and_then(|v| v.as_str()).unwrap_or("");
    format!("{}\t{}\t{}", c, t, r)
}

fn k_reaction(rec: &Value) -> String {
    let c = rec.get("channel_id").and_then(|v| v.as_str()).unwrap_or("");
    let m = rec.get("message_ts").and_then(|v| v.as_str()).unwrap_or("");
    let t = rec
        .get("thread_ts")
        .and_then(|v| v.as_str())
        .unwrap_or("__none__");
    format!("{}\t{}\t{}", c, m, t)
}

// ---------------------------------------------------------------------------
// Fetch helpers
// ---------------------------------------------------------------------------

fn datetime_to_slack_ts(dt: &DateTime<Utc>) -> String {
    let secs = dt.timestamp();
    let nanos = dt.timestamp_subsec_micros();
    format!("{}.{:06}", secs, nanos)
}

fn empty_params() -> BTreeMap<String, String> {
    BTreeMap::new()
}

async fn fetch_self(out_dir: &Path) -> Result<Value> {
    let data = call_slack("auth.test", &empty_params())
        .await
        .map_err(|e: SlackError| anyhow::anyhow!("{}", e))?;
    let user_id = data
        .get("user_id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let user_name = data
        .get("user")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let rec = make_record(
        &[
            ("user_id", Value::String(user_id.clone())),
            ("user_name", Value::String(user_name)),
        ],
        data,
    );
    let existing = load_latest_by_key(out_dir, ENTITY_SELF, k_user)?;
    diff_and_save(
        out_dir,
        ENTITY_SELF,
        std::slice::from_ref(&rec),
        &existing,
        k_user,
    )?;
    Ok(rec)
}

async fn fetch_channels(
    out_dir: &Path,
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
    let mut raws = paginate("conversations.list", &params, "channels").await?;
    if members_only {
        raws.retain(|c| {
            c.get("is_member")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
        });
    }
    let records: Vec<Value> = raws
        .into_iter()
        .map(|c| {
            let id = c
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            let name = c
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            make_record(
                &[
                    ("channel_id", Value::String(id)),
                    ("channel_name", Value::String(name)),
                ],
                c,
            )
        })
        .collect();
    let existing = load_latest_by_key(out_dir, ENTITY_CHANNEL, k_channel)?;
    diff_and_save(out_dir, ENTITY_CHANNEL, &records, &existing, k_channel)?;
    Ok(records)
}

async fn fetch_users(out_dir: &Path) -> Result<()> {
    let mut params = BTreeMap::new();
    params.insert("limit".to_string(), "200".to_string());
    let raws = paginate("users.list", &params, "members").await?;
    let records: Vec<Value> = raws
        .into_iter()
        .map(|u| {
            let id = u
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            make_record(&[("user_id", Value::String(id))], u)
        })
        .collect();
    let existing = load_latest_by_key(out_dir, ENTITY_USER, k_user)?;
    diff_and_save(out_dir, ENTITY_USER, &records, &existing, k_user)?;
    Ok(())
}

async fn fetch_history(
    channel_id: &str,
    oldest_ts: &str,
    inclusive: bool,
    latest_ts: Option<&str>,
) -> Result<Vec<Value>> {
    let mut params = BTreeMap::new();
    params.insert("channel".to_string(), channel_id.to_string());
    params.insert("oldest".to_string(), oldest_ts.to_string());
    params.insert(
        "inclusive".to_string(),
        if inclusive { "true" } else { "false" }.to_string(),
    );
    params.insert("include_all_metadata".to_string(), "true".to_string());
    params.insert("limit".to_string(), "200".to_string());
    if let Some(l) = latest_ts {
        params.insert("latest".to_string(), l.to_string());
    }
    paginate("conversations.history", &params, "messages").await
}

async fn fetch_replies(channel_id: &str, thread_ts: &str) -> Result<Vec<Value>> {
    let mut params = BTreeMap::new();
    params.insert("channel".to_string(), channel_id.to_string());
    params.insert("ts".to_string(), thread_ts.to_string());
    params.insert("limit".to_string(), "200".to_string());
    paginate("conversations.replies", &params, "messages").await
}

fn make_message_records(channel_id: &str, channel_name: &str, raws: Vec<Value>) -> Vec<Value> {
    raws.into_iter()
        .filter_map(|raw| {
            let ts = raw.get("ts").and_then(|v| v.as_str())?.to_string();
            Some(make_record(
                &[
                    ("channel_id", Value::String(channel_id.to_string())),
                    ("channel_name", Value::String(channel_name.to_string())),
                    ("message_ts", Value::String(ts)),
                ],
                raw,
            ))
        })
        .collect()
}

fn make_reply_records(
    channel_id: &str,
    channel_name: &str,
    thread_ts: &str,
    raws: Vec<Value>,
) -> Vec<Value> {
    raws.into_iter()
        .filter_map(|raw| {
            let ts = raw.get("ts").and_then(|v| v.as_str())?.to_string();
            if ts == thread_ts {
                return None;
            }
            Some(make_record(
                &[
                    ("channel_id", Value::String(channel_id.to_string())),
                    ("channel_name", Value::String(channel_name.to_string())),
                    ("thread_ts", Value::String(thread_ts.to_string())),
                    ("reply_ts", Value::String(ts)),
                ],
                raw,
            ))
        })
        .collect()
}

fn extract_reactions(
    channel_id: &str,
    channel_name: &str,
    message_records: &[Value],
    thread_ts: Option<&str>,
) -> Vec<Value> {
    let mut out = Vec::new();
    for r in message_records {
        let reactions = match r.get("raw").and_then(|v| v.get("reactions")) {
            Some(v) if !v.is_null() => v.clone(),
            _ => continue,
        };
        let msg_ts = match r
            .get("raw")
            .and_then(|raw| raw.get("ts"))
            .and_then(|v| v.as_str())
        {
            Some(s) => s.to_string(),
            None => continue,
        };
        let thread_val = match thread_ts {
            Some(s) => Value::String(s.to_string()),
            None => Value::Null,
        };
        out.push(make_record(
            &[
                ("channel_id", Value::String(channel_id.to_string())),
                ("channel_name", Value::String(channel_name.to_string())),
                ("message_ts", Value::String(msg_ts)),
                ("thread_ts", thread_val),
            ],
            json!({ "reactions": reactions }),
        ));
    }
    out
}

fn channel_latest_ts_map(existing_messages: &BTreeMap<String, Value>) -> BTreeMap<String, String> {
    let mut latest: BTreeMap<String, String> = BTreeMap::new();
    for key in existing_messages.keys() {
        let mut parts = key.split('\t');
        let cid = parts.next().unwrap_or("").to_string();
        let ts = parts.next().unwrap_or("").to_string();
        let entry = latest.entry(cid).or_default();
        if ts.as_str() > entry.as_str() {
            *entry = ts;
        }
    }
    latest
}

fn latest_reply_ts_map(
    existing_replies: &BTreeMap<String, Value>,
) -> BTreeMap<(String, String), String> {
    let mut latest: BTreeMap<(String, String), String> = BTreeMap::new();
    for key in existing_replies.keys() {
        let mut parts = key.split('\t');
        let cid = parts.next().unwrap_or("").to_string();
        let tts = parts.next().unwrap_or("").to_string();
        let rts = parts.next().unwrap_or("").to_string();
        let k = (cid, tts);
        let entry = latest.entry(k).or_default();
        if rts.as_str() > entry.as_str() {
            *entry = rts;
        }
    }
    latest
}

// ---------------------------------------------------------------------------
// Per-channel export.
// ---------------------------------------------------------------------------

#[allow(clippy::too_many_arguments)]
async fn export_channel(
    out_dir: &Path,
    channel_id: &str,
    channel_name: &str,
    since_ts: &str,
    refresh_window_days: i64,
    existing_messages: &BTreeMap<String, Value>,
    existing_replies: &BTreeMap<String, Value>,
    existing_reactions: &BTreeMap<String, Value>,
    channel_latest_ts: Option<&str>,
    latest_reply_by_thread: &BTreeMap<(String, String), String>,
    now: &DateTime<Utc>,
    media_dir: Option<&Path>,
    media_headers: Option<&BTreeMap<String, String>>,
) -> Result<(usize, usize, usize, BTreeMap<String, usize>)> {
    let (forward_oldest, inclusive) = match channel_latest_ts {
        Some(ts) => (ts.to_string(), false),
        None => (since_ts.to_string(), true),
    };

    let forward_raws = fetch_history(channel_id, &forward_oldest, inclusive, None).await?;
    let forward_records = make_message_records(channel_id, channel_name, forward_raws);

    let mut refresh_records: Vec<Value> = Vec::new();
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
                let raws = fetch_history(channel_id, &effective, true, Some(latest_ts)).await?;
                refresh_records = make_message_records(channel_id, channel_name, raws);
            }
        }
    }

    let mut combined_msgs: Vec<Value> = forward_records.clone();
    combined_msgs.extend(refresh_records.clone());

    let (new_msgs, _) = diff_and_save(
        out_dir,
        ENTITY_MESSAGE,
        &combined_msgs,
        existing_messages,
        k_message,
    )?;

    // Identify parent threads to fetch replies for.
    let mut seen_ts: BTreeSet<String> = BTreeSet::new();
    let mut parents: Vec<Value> = Vec::new();
    for r in &combined_msgs {
        let ts = match r
            .get("raw")
            .and_then(|raw| raw.get("ts"))
            .and_then(|v| v.as_str())
        {
            Some(s) => s.to_string(),
            None => continue,
        };
        if !seen_ts.insert(ts.clone()) {
            continue;
        }
        let reply_count = r
            .get("raw")
            .and_then(|raw| raw.get("reply_count"))
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        if reply_count > 0 {
            parents.push(r.clone());
        }
    }

    let mut new_reply_count = 0usize;
    let mut all_reply_records: Vec<Value> = Vec::new();
    for parent in &parents {
        let thread_ts = match parent
            .get("raw")
            .and_then(|raw| raw.get("ts"))
            .and_then(|v| v.as_str())
        {
            Some(s) => s.to_string(),
            None => continue,
        };
        let api_latest = parent
            .get("raw")
            .and_then(|raw| raw.get("latest_reply"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let stored_latest =
            latest_reply_by_thread.get(&(channel_id.to_string(), thread_ts.clone()));
        if let (Some(api), Some(stored)) =
            (api_latest.as_deref(), stored_latest.map(|s| s.as_str()))
        {
            if stored >= api {
                continue;
            }
        }
        let raws = fetch_replies(channel_id, &thread_ts).await?;
        let reply_records = make_reply_records(channel_id, channel_name, &thread_ts, raws);
        all_reply_records.extend(reply_records.clone());
        let (n_new, _) = diff_and_save(
            out_dir,
            ENTITY_REPLY,
            &reply_records,
            existing_replies,
            k_reply,
        )?;
        new_reply_count += n_new;
    }

    let msg_reactions = extract_reactions(channel_id, channel_name, &combined_msgs, None);
    let mut by_thread: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for rr in &all_reply_records {
        let tts = rr
            .get("thread_ts")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        by_thread.entry(tts).or_default().push(rr.clone());
    }
    let mut reply_reactions: Vec<Value> = Vec::new();
    for (tts, replies) in &by_thread {
        reply_reactions.extend(extract_reactions(
            channel_id,
            channel_name,
            replies,
            Some(tts),
        ));
    }
    let mut all_reactions = msg_reactions;
    all_reactions.extend(reply_reactions);
    let (new_react, _) = diff_and_save(
        out_dir,
        ENTITY_REACTION,
        &all_reactions,
        existing_reactions,
        k_reaction,
    )?;

    let mut media_counts: BTreeMap<String, usize> = BTreeMap::new();
    if let (Some(md), Some(mh)) = (media_dir, media_headers) {
        let mut all_with_files: Vec<Value> = combined_msgs.clone();
        all_with_files.extend(all_reply_records.clone());
        media_counts = api::download_files_for_records(&all_with_files, md, mh).await;
    }

    Ok((new_msgs, new_reply_count, new_react, media_counts))
}

// ---------------------------------------------------------------------------
// Public entry point — used by HTTP /api/sync/jobs.
// ---------------------------------------------------------------------------

pub struct FetchOptions {
    pub out_dir: PathBuf,
    pub channels: Option<Vec<String>>,
    pub since: String,
    pub refresh_window_days: i64,
    pub members_only: bool,
    pub media: bool,
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
        }
    }
}

pub struct FetchSummary {
    pub messages: usize,
    pub replies: usize,
    pub reactions: usize,
    pub media: BTreeMap<String, usize>,
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    std::fs::create_dir_all(&opts.out_dir)
        .with_context(|| format!("mkdir -p {}", opts.out_dir.display()))?;

    let since_dt =
        parse_iso_or_utc_date(&opts.since).with_context(|| format!("--since {:?}", opts.since))?;
    let since_ts = datetime_to_slack_ts(&since_dt);
    let now = Utc::now();

    let out = &opts.out_dir;
    let existing_channels = load_latest_by_key(out, ENTITY_CHANNEL, k_channel)?;
    let existing_users = load_latest_by_key(out, ENTITY_USER, k_user)?;
    let existing_messages = load_latest_by_key(out, ENTITY_MESSAGE, k_message)?;
    let existing_replies = load_latest_by_key(out, ENTITY_REPLY, k_reply)?;
    let existing_reactions = load_latest_by_key(out, ENTITY_REACTION, k_reaction)?;

    let channel_latest_ts = channel_latest_ts_map(&existing_messages);
    let latest_reply_by_thread = latest_reply_ts_map(&existing_replies);

    eprintln!(
        "[slack] existing: channels={} users={} messages={} replies={} reactions={}",
        existing_channels.len(),
        existing_users.len(),
        existing_messages.len(),
        existing_replies.len(),
        existing_reactions.len()
    );

    let _self_rec = fetch_self(out).await?;

    let fresh_channels = fetch_channels(out, opts.members_only, opts.channels.is_some()).await?;

    let mut name_to_id: BTreeMap<String, String> = BTreeMap::new();
    for c in &fresh_channels {
        if let (Some(n), Some(i)) = (
            c.get("channel_name").and_then(|v| v.as_str()),
            c.get("channel_id").and_then(|v| v.as_str()),
        ) {
            name_to_id.insert(n.to_string(), i.to_string());
        }
    }
    for prior in existing_channels.values() {
        if let (Some(n), Some(i)) = (
            prior.get("channel_name").and_then(|v| v.as_str()),
            prior.get("channel_id").and_then(|v| v.as_str()),
        ) {
            name_to_id
                .entry(n.to_string())
                .or_insert_with(|| i.to_string());
        }
    }

    fetch_users(out).await?;

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
                let n = c.get("channel_name").and_then(|v| v.as_str())?;
                let i = c.get("channel_id").and_then(|v| v.as_str())?;
                Some((i.to_string(), n.to_string()))
            })
            .collect(),
    };

    let media_dir: Option<PathBuf> = if opts.media {
        Some(out.join("media"))
    } else {
        None
    };
    let media_headers = if opts.media {
        Some(api::extract_file_auth().await?)
    } else {
        None
    };

    eprintln!(
        "[slack] exporting {} channels (media: {})",
        targets.len(),
        if opts.media { "on" } else { "off" }
    );

    let mut totals = FetchSummary {
        messages: 0,
        replies: 0,
        reactions: 0,
        media: BTreeMap::new(),
    };
    for (cid, name) in &targets {
        eprintln!("[slack] {}", name);
        match export_channel(
            out,
            cid,
            name,
            &since_ts,
            opts.refresh_window_days,
            &existing_messages,
            &existing_replies,
            &existing_reactions,
            channel_latest_ts.get(cid).map(|s| s.as_str()),
            &latest_reply_by_thread,
            &now,
            media_dir.as_deref(),
            media_headers.as_ref(),
        )
        .await
        {
            Ok((n_msg, n_reply, n_react, m_counts)) => {
                totals.messages += n_msg;
                totals.replies += n_reply;
                totals.reactions += n_react;
                for (k, v) in m_counts {
                    *totals.media.entry(k).or_insert(0) += v;
                }
            }
            Err(e) => eprintln!("[slack] ! {}: {}", name, e),
        }
    }

    eprintln!(
        "[slack] new: {} messages  {} replies  {} reactions",
        totals.messages, totals.replies, totals.reactions
    );
    Ok(totals)
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
