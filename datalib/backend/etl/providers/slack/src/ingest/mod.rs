//! Slack downloader entry point.

pub mod api;
pub mod db;
pub mod files;
pub mod schema_raw;
pub mod shapes;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use serde_json::{json, Value};
use sqlx::{Sqlite, Transaction};
use tracing::{info, info_span, instrument, warn, Instrument};

use api::{call_slack, SlackCall, SlackError};
use async_trait::async_trait;
use datalib_etl::bulk::BulkUpsertable;
use datalib_etl::download_problems::{DownloadProblem, RunProblem};
use datalib_etl::events;
use datalib_etl::progress::RunBar;
use datalib_etl::raw_store::Sealer;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl::stop::StopFlag;
use datalib_etl_web::coverage::{self, Span};
use datalib_etl_web::http::LatchkeySettings;
use datalib_etl_web::owed::{self, BatchError, Fetched, Fetcher, Listed, Outcome};
pub use db::{
    block_on_load_all, db_path_for, Enumerated, FetchTarget, LoadedMessage, LoadedRaw,
    MessageInput, OwedFile, RawDb, Thread, UserDirectoryEntry,
};
use files::{FileFetcher, FILE_BATCH};
use schema_raw::{SlackAttachmentRow, THREADS};
use shapes::{
    M_AUTH_TEST, M_BOOKMARKS, M_CHANNELS, M_COUNTS, M_HISTORY, M_REPLIES, M_SAVED, M_USERS,
};

pub const DEFAULT_SINCE: &str = "2024-01-01";
pub const DEFAULT_REFRESH_WINDOW_DAYS: i64 = 30;

/// Max age of a successful channel/user list sweep before we refetch.
/// Slack `conversations.list` is Tier-2 rate-limited (~20 req/min), so
/// a workspace with thousands of channels costs tens of seconds per
/// refetch even on warm-cache runs.
pub const MANIFEST_TTL: chrono::Duration = chrono::Duration::hours(6);

// Per-method drivers.

fn datetime_to_slack_ts(dt: &DateTime<Utc>) -> String {
    // Slack answers a negative `oldest` with an empty page, and a
    // negative `ts` does not sort as a `coverage` bound. No message is
    // older than the epoch, so an earlier instant asks for the same.
    let dt = (*dt).max(DateTime::UNIX_EPOCH);
    format!("{}.{:06}", dt.timestamp(), dt.timestamp_subsec_micros())
}

fn days_before(now: DateTime<Utc>, days: i64) -> DateTime<Utc> {
    ChronoDuration::try_days(days)
        .and_then(|window| now.checked_sub_signed(window))
        .unwrap_or(DateTime::<Utc>::MIN_UTC)
}

fn empty_params() -> BTreeMap<String, String> {
    BTreeMap::new()
}

async fn call(
    method: &str,
    params: &BTreeMap<String, String>,
    latchkey: &LatchkeySettings,
) -> Result<Value> {
    let SlackCall { response, .. } = call_slack(method, params, latchkey)
        .await
        .map_err(anyhow::Error::from)?;
    Ok(response)
}

/// `(team_id, self_user_id)` from `auth.test`. `self_user_id` is who
/// the credential belongs to — needed to subtract the account itself
/// out of a group DM's `members` when naming it. Optional because only
/// the DM path needs it and a missing field must not sink a sync.
#[instrument(skip_all)]
async fn fetch_self(
    db: &RawDb,
    progress: &datalib_etl::progress::Progress,
    latchkey: &LatchkeySettings,
) -> Result<(String, Option<String>)> {
    progress.set_message("auth.test");
    let t0 = std::time::Instant::now();
    let resp = call(M_AUTH_TEST, &empty_params(), latchkey).await?;
    db.upsert_workspace(&resp).await?;
    let team_id = resp
        .get("team_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("auth.test response missing team_id"))?
        .to_string();
    let self_user_id = resp
        .get("user_id")
        .and_then(|v| v.as_str())
        .map(String::from);
    info!(
        event = "slack_fetch_self_done",
        team_id = %team_id,
        elapsed_ms = t0.elapsed().as_millis() as u64,
        "fetched the workspace identity"
    );
    Ok((team_id, self_user_id))
}

/// `types` for `conversations.list`. `im` / `mpim` are appended only
/// when DMs are wanted: the parameter is what decides whether Slack
/// hands us DM conversations at all, so leaving it at the channel pair
/// is the enforcement point for `dms = false`, not just a filter.
pub(crate) fn conversation_types(dms: bool) -> &'static str {
    if dms {
        "public_channel,private_channel,im,mpim"
    } else {
        "public_channel,private_channel"
    }
}

#[instrument(skip(db, now, progress, latchkey))]
async fn fetch_channels(
    db: &RawDb,
    members_only: bool,
    include_archived: bool,
    dms: bool,
    now: &DateTime<Utc>,
    progress: &datalib_etl::progress::Progress,
    latchkey: &LatchkeySettings,
) -> Result<Vec<FetchTarget>> {
    // `dms` is part of the key, not just the request: turning DMs on
    // asks for a strictly wider `types`, and a sweep recorded under the
    // narrower one would suppress the refetch for up to MANIFEST_TTL —
    // the run would then find no DM rows and quietly mirror nothing.
    let sweep_key = format!("channels:archived={include_archived}:dms={dms}");
    if let Some(age) = db.manifest_sweep_age(&sweep_key, now).await? {
        if age < MANIFEST_TTL {
            let age_s = age.num_seconds().max(0);
            info!(
                event = "slack_fetch_channels_skipped",
                reason = "ttl",
                age_s = age_s,
                ttl_s = MANIFEST_TTL.num_seconds(),
                "the channel listing is fresh enough; not re-listing"
            );
            progress.set_message(&format!(
                "conversations.list cached ({age_s}s old, TTL {}s)",
                MANIFEST_TTL.num_seconds()
            ));
            return db
                .channels_for_fetch(members_only, include_archived, dms)
                .await;
        }
    }

    let mut params = BTreeMap::new();
    params.insert(
        "exclude_archived".to_string(),
        if include_archived { "false" } else { "true" }.to_string(),
    );
    params.insert("limit".to_string(), "200".to_string());
    params.insert("types".to_string(), conversation_types(dms).to_string());

    let t0 = std::time::Instant::now();
    progress.set_message("conversations.list page 1");
    let mut cursor: Option<String> = None;
    let mut pages = 0u64;
    let mut total = 0usize;
    loop {
        let mut p = params.clone();
        if let Some(c) = &cursor {
            p.insert("cursor".to_string(), c.clone());
        }
        let resp = call(M_CHANNELS, &p, latchkey).await?;
        if let Some(arr) = resp.get("channels").and_then(|v| v.as_array()) {
            db.upsert_channels(arr).await?;
            total += arr.len();
        }
        pages += 1;
        progress.set_message(&format!(
            "conversations.list page {pages} ({total} channels so far)"
        ));
        cursor = next_cursor(&resp);
        if cursor.is_none() || resp.get("has_more").and_then(|v| v.as_bool()) == Some(false) {
            break;
        }
    }
    info!(
        event = "slack_fetch_channels_done",
        pages = pages,
        channels = total,
        elapsed_ms = t0.elapsed().as_millis() as u64,
        "listed the channels"
    );
    db.record_manifest_sweep(&sweep_key, now).await?;
    db.channels_for_fetch(members_only, include_archived, dms)
        .await
}

#[instrument(skip_all)]
async fn fetch_users(
    db: &RawDb,
    now: &DateTime<Utc>,
    progress: &datalib_etl::progress::Progress,
    latchkey: &LatchkeySettings,
) -> Result<usize> {
    let sweep_key = "users";
    if let Some(age) = db.manifest_sweep_age(sweep_key, now).await? {
        if age < MANIFEST_TTL {
            let age_s = age.num_seconds().max(0);
            info!(
                event = "slack_fetch_users_skipped",
                reason = "ttl",
                age_s = age_s,
                ttl_s = MANIFEST_TTL.num_seconds(),
                "the user listing is fresh enough; not re-listing"
            );
            progress.set_message(&format!(
                "users.list cached ({age_s}s old, TTL {}s)",
                MANIFEST_TTL.num_seconds()
            ));
            return Ok(0);
        }
    }

    let mut base = BTreeMap::new();
    base.insert("limit".to_string(), "200".to_string());
    let t0 = std::time::Instant::now();
    progress.set_message("users.list page 1");
    let mut cursor: Option<String> = None;
    let mut count = 0usize;
    let mut pages = 0u64;
    loop {
        let mut p = base.clone();
        if let Some(c) = &cursor {
            p.insert("cursor".to_string(), c.clone());
        }
        let resp = call(M_USERS, &p, latchkey).await?;
        if let Some(arr) = resp.get("members").and_then(|v| v.as_array()) {
            db.upsert_users(arr).await?;
            count += arr.len();
        }
        pages += 1;
        progress.set_message(&format!("users.list page {pages} ({count} users so far)"));
        cursor = next_cursor(&resp);
        if cursor.is_none() {
            break;
        }
    }
    let elapsed_ms = t0.elapsed().as_millis() as u64;
    events::indexed_batch("users", count, elapsed_ms);
    info!(
        event = "slack_fetch_users_done",
        pages = pages,
        users = count,
        elapsed_ms = elapsed_ms,
        "listed the users"
    );
    db.record_manifest_sweep(sweep_key, now).await?;
    Ok(count)
}

pub(crate) fn next_cursor(resp: &Value) -> Option<String> {
    resp.get("response_metadata")
        .and_then(|m| m.get("next_cursor"))
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

// The account's own place in the workspace: how far it has read, what
// each conversation has bookmarked, what it saved for later.

/// Filters `saved.list` walks, one listing each. With no `filter` it
/// answers only what `saved` does; the three together are every item
/// (checked against the response's own `counts.total_count` on
/// 2026-09-24).
const SAVED_FILTERS: &[&str] = &["saved", "completed", "archived"];

fn refused(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<SlackError>(),
        Some(SlackError::Refused { .. })
    )
}

fn interrupted(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<SlackError>(),
        Some(SlackError::Interrupted(_))
    )
}

fn listing_problem(name: &str, e: &anyhow::Error) -> RunProblem {
    problem_for(name, format!("{e:#}"), refused(e))
}

fn problem_for(name: &str, detail: String, refused: bool) -> RunProblem {
    if refused {
        RunProblem::forbidden(name, detail)
    } else {
        RunProblem::listing(name, detail)
    }
}

#[derive(Debug, Default, serde::Serialize)]
pub struct AccountTotals {
    pub read_states: usize,
    pub bookmarks: usize,
    pub saved_items: usize,
    /// Bookmarks and saved items upstream no longer lists.
    pub pruned: usize,
}

/// `client.counts` has one entry per conversation the account is in,
/// mirrored or not; only the mirrored ones are kept, so `dms = false`
/// keeps DMs out of this table as it does out of `channels`.
async fn fetch_read_states(
    db: &RawDb,
    in_scope: &HashSet<String>,
    latchkey: &LatchkeySettings,
) -> Result<usize> {
    let resp = call(M_COUNTS, &empty_params(), latchkey).await?;
    let mut entries: Vec<Value> = Vec::new();
    for surface in ["channels", "mpims", "ims"] {
        let listed = resp
            .get(surface)
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow::anyhow!("{M_COUNTS}: no `{surface}` array"))?;
        entries.extend(
            listed
                .iter()
                .filter(|e| {
                    e.get("id")
                        .and_then(Value::as_str)
                        .is_some_and(|id| in_scope.contains(id))
                })
                .cloned(),
        );
    }
    db.upsert_read_states(&entries).await?;
    Ok(entries.len())
}

/// Every page of every [`SAVED_FILTERS`] listing, or an error: a walk
/// that stopped short is not an enumeration, and nothing may be pruned
/// to it.
async fn list_saved_items(latchkey: &LatchkeySettings) -> Result<Vec<Value>> {
    let mut items = Vec::new();
    for filter in SAVED_FILTERS {
        let mut cursor: Option<String> = None;
        loop {
            let mut p = BTreeMap::new();
            p.insert("filter".to_string(), filter.to_string());
            p.insert("limit".to_string(), "50".to_string());
            if let Some(c) = &cursor {
                p.insert("cursor".to_string(), c.clone());
            }
            let resp = call(M_SAVED, &p, latchkey).await?;
            let page = resp
                .get("saved_items")
                .and_then(Value::as_array)
                .ok_or_else(|| anyhow::anyhow!("{M_SAVED} filter={filter}: no `saved_items`"))?;
            items.extend(page.iter().cloned());
            cursor = next_cursor(&resp);
            if cursor.is_none() {
                break;
            }
        }
    }
    Ok(items)
}

/// Saved items in conversations this run mirrors. Everything else in
/// the listing points at messages we do not hold.
fn saved_items_in_scope(items: Vec<Value>, in_scope: &HashSet<String>) -> Vec<Value> {
    items
        .into_iter()
        .filter(|i| {
            i.get("item_id")
                .and_then(Value::as_str)
                .is_some_and(|id| in_scope.contains(id))
        })
        .collect()
}

/// `bookmarks.list` for each mirrored conversation whose header has a
/// bookmarks bar (see [`RawDb::channels_to_list_bookmarks`]): on a real
/// workspace 75 of 128 channels had none, and none of the 25 of those
/// asked held a bookmark. At most once per [`MANIFEST_TTL`], like the
/// channel listing. Returns `(stored, pruned)`; a conversation that
/// could not be listed keeps what it had.
async fn fetch_bookmarks(
    db: &RawDb,
    targets: &[(String, String)],
    now: &DateTime<Utc>,
    stop: &datalib_etl::stop::StopFlag,
    progress: &datalib_etl::progress::Progress,
    latchkey: &LatchkeySettings,
    problems: &mut Vec<RunProblem>,
) -> Result<(usize, usize)> {
    let sweep_key = "bookmarks";
    if let Some(age) = db.manifest_sweep_age(sweep_key, now).await? {
        if age < MANIFEST_TTL {
            info!(
                event = "slack_fetch_bookmarks_skipped",
                reason = "ttl",
                age_s = age.num_seconds().max(0),
                "the bookmarks are fresh enough; not re-listing"
            );
            return Ok((0, 0));
        }
    }
    let with_bar = db.channels_to_list_bookmarks().await?;
    let mut stored = 0usize;
    let mut pruned = 0usize;
    let mut failed: Vec<(String, anyhow::Error)> = Vec::new();
    let mut stopped = false;
    for (cid, label) in targets.iter().filter(|(cid, _)| with_bar.contains(cid)) {
        if stop.requested() {
            stopped = true;
            break;
        }
        progress.set_message(&format!("{M_BOOKMARKS} {label}"));
        let mut p = BTreeMap::new();
        p.insert("channel_id".to_string(), cid.clone());
        let listed = call(M_BOOKMARKS, &p, latchkey).await.and_then(|resp| {
            resp.get("bookmarks")
                .and_then(Value::as_array)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("{M_BOOKMARKS} {label}: no `bookmarks` array"))
        });
        match listed {
            Ok(bookmarks) => {
                stored += bookmarks.len();
                pruned += db.replace_bookmarks(cid, &bookmarks).await?;
            }
            Err(e) => {
                warn!(event = "slack_bookmarks_failed", channel = %label, error = %format!("{e:#}"), "could not list a conversation's bookmarks");
                failed.push((label.clone(), e));
            }
        }
    }
    if let Some((label, first)) = failed.first() {
        let detail = format!(
            "{} of the conversations with bookmarks could not be listed; the first, {label}: {first:#}",
            failed.len()
        );
        problems.push(problem_for(M_BOOKMARKS, detail, refused(first)));
    } else if !stopped {
        db.record_manifest_sweep(sweep_key, now).await?;
    }
    Ok((stored, pruned))
}

/// Read states, saved items and bookmarks for the conversations this run
/// mirrors. Each listing that fails becomes a `problems` row and costs
/// only its own table; none of them sinks the message walk.
async fn fetch_account_state(
    db: &RawDb,
    targets: &[(String, String)],
    now: &DateTime<Utc>,
    stop: &datalib_etl::stop::StopFlag,
    progress: &datalib_etl::progress::Progress,
    latchkey: &LatchkeySettings,
) -> Result<(AccountTotals, Vec<RunProblem>)> {
    let in_scope: HashSet<String> = targets.iter().map(|(cid, _)| cid.clone()).collect();
    let mut totals = AccountTotals::default();
    let mut problems = Vec::new();

    progress.set_message(M_COUNTS);
    match fetch_read_states(db, &in_scope, latchkey).await {
        Ok(n) => totals.read_states = n,
        Err(e) => problems.push(listing_problem(M_COUNTS, &e)),
    }

    progress.set_message(M_SAVED);
    match list_saved_items(latchkey).await {
        Ok(items) => {
            let listed = items.len();
            let kept = saved_items_in_scope(items, &in_scope);
            totals.saved_items = kept.len();
            totals.pruned += db.replace_saved_items(&kept, &in_scope).await?;
            info!(
                event = "slack_saved_items_listed",
                listed,
                kept = kept.len(),
                "listed the saved-for-later items; kept those in mirrored conversations"
            );
        }
        Err(e) => problems.push(listing_problem(M_SAVED, &e)),
    }

    let (bookmarks, pruned) =
        fetch_bookmarks(db, targets, now, stop, progress, latchkey, &mut problems).await?;
    totals.bookmarks = bookmarks;
    totals.pruned += pruned;

    info!(
        event = "slack_account_state_done",
        read_states = totals.read_states,
        saved_items = totals.saved_items,
        bookmarks = totals.bookmarks,
        pruned = totals.pruned,
        problems = problems.len(),
        "recorded read states, saved items and bookmarks"
    );
    Ok((totals, problems))
}

// Which conversations this run walks.

/// Does this read as one of Slack's conversation ids? A capital
/// `C`/`D`/`G` and then upper-case alphanumerics — the shape of every
/// channel, DM and group-DM id Slack has ever issued.
fn looks_like_conversation_id(s: &str) -> bool {
    let mut chars = s.chars();
    matches!(chars.next(), Some('C' | 'D' | 'G'))
        && s.len() >= 2
        && chars.all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
}

/// A `dm_conversations` entry as the id it names: the bare id, or the
/// id inside a pasted Slack link — `Copy link` on a DM gives
/// `https://<ws>.slack.com/archives/D…`, the app's own address bar
/// `https://app.slack.com/client/T…/D…`, and a message link puts a
/// `p<ts>` after the id. Anything else is handed back trimmed, so an
/// unmatched entry is reported in the person's own words.
pub fn conversation_id(spec: &str) -> String {
    let spec = spec.trim();
    if !(spec.starts_with("http://") || spec.starts_with("https://")) {
        return spec.to_string();
    }
    let path = spec.split(['?', '#']).next().unwrap_or(spec);
    path.split('/')
        .find(|seg| looks_like_conversation_id(seg))
        .unwrap_or(spec)
        .to_string()
}

/// What [`select_targets`] decided.
#[derive(Debug, Default, PartialEq, Eq)]
struct TargetPlan {
    /// `(channel_id, label)` for every conversation to walk. Channels
    /// first, then DMs.
    targets: Vec<(String, String)>,
    /// How many of `targets` are DMs — the tail of the vec.
    dm_targets: usize,
    /// `channels` entries that matched no channel. Reported rather
    /// than ignored: silence here is indistinguishable from "that
    /// channel has no messages".
    unmatched: Vec<String>,
    /// `dm_conversations` entries that matched no DM, for the same
    /// reason.
    unmatched_dms: Vec<String>,
}

fn select_targets(
    listed: &[FetchTarget],
    channels: Option<&[String]>,
    dm_conversations: Option<&[String]>,
    user_labels: &BTreeMap<String, String>,
    self_user_id: Option<&str>,
) -> TargetPlan {
    let mut plan = TargetPlan::default();

    let by_name: BTreeMap<&str, &FetchTarget> = listed
        .iter()
        .filter(|t| !t.is_dm)
        .filter_map(|t| t.name.as_deref().map(|n| (n, t)))
        .collect();

    match channels {
        Some(specs) => {
            for spec in specs {
                let name = spec.trim().trim_start_matches('#');
                match by_name.get(name) {
                    Some(t) => plan.targets.push((t.id.clone(), name.to_string())),
                    None => plan.unmatched.push(spec.clone()),
                }
            }
        }
        None => {
            for t in listed.iter().filter(|t| !t.is_dm) {
                let label = t.name.clone().unwrap_or_else(|| t.id.clone());
                plan.targets.push((t.id.clone(), label));
            }
        }
    }

    let dm_label = |t: &FetchTarget| {
        let counterparts = schema_raw::dm_counterparts(&t.dm_user_ids, self_user_id);
        schema_raw::dm_display_name(&counterparts, t.name.as_deref(), &t.id, user_labels)
    };
    match dm_conversations {
        Some(specs) => {
            let by_id: BTreeMap<&str, &FetchTarget> = listed
                .iter()
                .filter(|t| t.is_dm)
                .map(|t| (t.id.as_str(), t))
                .collect();
            for spec in specs {
                match by_id.get(conversation_id(spec).as_str()) {
                    Some(t) => {
                        plan.targets.push((t.id.clone(), dm_label(t)));
                        plan.dm_targets += 1;
                    }
                    None => plan.unmatched_dms.push(spec.clone()),
                }
            }
        }
        None => {
            for t in listed.iter().filter(|t| t.is_dm) {
                plan.targets.push((t.id.clone(), dm_label(t)));
                plan.dm_targets += 1;
            }
        }
    }

    plan
}

// One channel: its history, then what the store says it still owes.

/// A `ts` as a `coverage` bound: padded, so bounds sort as instants do
/// whatever the width of the seconds.
fn ts_key(ts: &str) -> String {
    format!("{ts:0>19}")
}

fn key_ts(key: &str) -> String {
    let ts = key.trim_start_matches('0');
    if ts.is_empty() || ts.starts_with('.') {
        format!("0{ts}")
    } else {
        ts.to_string()
    }
}

/// A bound above every `ts`. The history wanted has no top: a walk
/// covers up to the newest message it saw, never up to "now", so what
/// is newer than that is asked for on every run and a message that
/// becomes visible late is not skipped.
const END_OF_TIME: &str = "999999999999.999999";

/// One walk of `conversations.history` over a stretch of a channel.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stretch {
    span: Span,
    /// Whether a message at either end is asked for. An end that is the
    /// end of a stretch already covered has been read.
    inclusive: bool,
    /// Covered before: read again for edits, and what it no longer
    /// lists is deleted.
    reread: bool,
}

/// What to walk of a channel that has `held` covered: the gaps in
/// `[since, ∞)`, newest first, then whatever was already covered from
/// `refresh_from` up.
fn stretches(since: &str, refresh_from: Option<&str>, held: &[Span]) -> Vec<Stretch> {
    let wanted = Span::new(since, END_OF_TIME);
    let mut out: Vec<Stretch> = coverage::gaps(&wanted, held)
        .into_iter()
        .rev()
        .map(|gap| Stretch {
            inclusive: gap.lo == since,
            span: gap,
            reread: false,
        })
        .collect();
    let Some(from) = refresh_from.map(|from| from.max(since)) else {
        return out;
    };
    for span in coverage::merged(held.to_vec()).into_iter().rev() {
        if span.hi.as_str() > from {
            out.push(Stretch {
                span: Span::new(span.lo.as_str().max(from), span.hi),
                inclusive: true,
                reread: true,
            });
        }
    }
    out
}

/// The part of `stretch` one page settles. History is newest first, so
/// a page reaches from where the page before left off (`top`; on the
/// first page of an open-topped stretch, the newest message it holds)
/// down to its own oldest message, and the last page down to the
/// stretch's bottom.
fn settled(stretch: &Span, top: Option<&str>, page: &[String], last: bool) -> Option<Span> {
    let hi = top.or(page.iter().max().map(String::as_str))?;
    let lo = if last {
        stretch.lo.as_str()
    } else {
        page.iter().min()?.as_str().max(stretch.lo.as_str())
    };
    (lo <= hi).then(|| Span::new(lo, hi))
}

/// `has_more` with no cursor to ask with: not a listing of anything.
fn cut_short(method: &str) -> anyhow::Error {
    anyhow::anyhow!("{method}: has_more with no cursor; the rest was not read")
}

#[derive(Default)]
struct ChannelTotals {
    messages: usize,
    replies: usize,
    /// Messages and replies Slack has stopped serving inside a range we
    /// read whole.
    pruned: usize,
    media: BTreeMap<String, usize>,
}

/// What every channel's mirroring shares.
struct Mirror<'a> {
    db: &'a RawDb,
    team_id: &'a str,
    since: String,
    refresh_from: Option<String>,
    media: bool,
    blob_size_limit_bytes: Option<u64>,
    bar: &'a RunBar,
    latchkey: &'a LatchkeySettings,
    stop: &'a StopFlag,
    found: &'a RunProblems,
    sealer: Option<&'a Sealer>,
    blake3_by_file: &'a Mutex<HashMap<String, String>>,
}

/// One channel's owed threads, for [`owed::drain`]: a thread is one
/// record whose fetch is paged.
struct Threads<'a> {
    mirror: &'a Mirror<'a>,
    channel_id: &'a str,
    /// Top-level messages the channel's walk stored, for the progress line.
    messages: usize,
    replies: AtomicUsize,
    pruned: AtomicUsize,
}

#[async_trait]
impl Fetcher<Thread> for Threads<'_> {
    async fn fetch(
        &self,
        batch: Vec<Listed>,
    ) -> std::result::Result<Vec<Fetched<Thread>>, BatchError> {
        let mut out = Vec::with_capacity(batch.len());
        for listed in batch {
            let fetched = self
                .mirror
                .fetch_thread(self.channel_id, &listed.key, &self.replies)
                .await;
            let outcome = match fetched {
                Ok(thread) => Outcome::Got(thread),
                // The loop reads a stop off its flag; every other error
                // is this thread's.
                Err(e) if interrupted(&e) => return Err(BatchError::Batch(e)),
                Err(e) => Outcome::Failed(format!("{e:#}")),
            };
            out.push(Fetched { listed, outcome });
            self.mirror.bar.doing(&format!(
                "msgs={} replies={}",
                self.messages,
                self.replies.load(Ordering::Relaxed)
            ));
        }
        Ok(out)
    }

    async fn store(
        &self,
        tx: &mut Transaction<'static, Sqlite>,
        batch: &[Fetched<Thread>],
    ) -> Result<()> {
        let gone = self.mirror.db.store_threads(tx, batch).await?;
        self.pruned.fetch_add(gone, Ordering::Relaxed);
        Ok(())
    }
}

impl Mirror<'_> {
    /// A history walk that fails still leaves the channel's owed threads
    /// and files to fetch: they are in the store whatever the walk did.
    async fn channel(&self, channel_id: &str, totals: &mut ChannelTotals) -> Result<()> {
        let walked = self.walk_history(channel_id, totals).await;
        if walked.as_ref().is_err_and(interrupted) {
            return walked;
        }
        self.fetch_owed_threads(channel_id, totals).await?;
        if self.media {
            self.fetch_owed_files(channel_id, totals).await?;
        }
        walked
    }

    async fn walk_history(&self, channel_id: &str, totals: &mut ChannelTotals) -> Result<()> {
        let held = coverage::held(self.db.pool(), &db::history_scope(channel_id)).await?;
        for stretch in stretches(&self.since, self.refresh_from.as_deref(), &held) {
            self.walk_stretch(channel_id, &stretch, totals).await?;
        }
        Ok(())
    }

    async fn walk_stretch(
        &self,
        channel_id: &str,
        stretch: &Stretch,
        totals: &mut ChannelTotals,
    ) -> Result<()> {
        let open_top = stretch.span.hi == END_OF_TIME;
        let mut base = BTreeMap::new();
        base.insert("channel".to_string(), channel_id.to_string());
        base.insert("oldest".to_string(), key_ts(&stretch.span.lo));
        base.insert("inclusive".to_string(), stretch.inclusive.to_string());
        base.insert("include_all_metadata".to_string(), "true".to_string());
        base.insert("limit".to_string(), "200".to_string());
        if !open_top {
            base.insert("latest".to_string(), key_ts(&stretch.span.hi));
        }

        // Where the next page reaches up to, and whether a message there
        // is that page's to return: the stretch's own top is, a page's
        // oldest message was the page before's.
        let mut top = (!open_top).then(|| stretch.span.hi.clone());
        let mut top_included = true;
        let mut cursor: Option<String> = None;
        loop {
            let mut params = base.clone();
            if let Some(c) = &cursor {
                params.insert("cursor".to_string(), c.clone());
            }
            let resp = call(M_HISTORY, &params, self.latchkey).await?;
            let messages: Vec<Value> = resp
                .get("messages")
                .and_then(|v| v.as_array())
                .map(|a| a.to_vec())
                .unwrap_or_default();
            info!(
                event = "slack_history_page",
                channel = %channel_id,
                oldest = params.get("oldest").map(String::as_str).unwrap_or("-"),
                latest = params.get("latest").map(String::as_str).unwrap_or("-"),
                inclusive = stretch.inclusive,
                reread = stretch.reread,
                cursor = params.get("cursor").map(String::as_str).unwrap_or("-"),
                returned = messages.len(),
                "fetched one page of history"
            );
            // Announced before it is counted: Slack names no message
            // count up front, so a page is the first this walk knows of
            // the work in it.
            self.bar.expect(messages.len() as u64);
            let rows: Vec<MessageInput> = messages
                .iter()
                .filter_map(|m| history_message_input(self.team_id, channel_id, m))
                .collect();

            let has_more = resp.get("has_more").and_then(|v| v.as_bool());
            cursor = next_cursor(&resp);
            let last = cursor.is_none() || has_more == Some(false);
            let short = cursor.is_none() && has_more == Some(true);
            let in_stretch: Vec<String> = rows
                .iter()
                .map(|m| ts_key(&m.ts))
                .filter(|key| *key >= stretch.span.lo)
                .collect();
            let covered =
                settled(&stretch.span, top.as_deref(), &in_stretch, last).filter(|_| !short);
            let enumerated = covered
                .as_ref()
                .filter(|_| stretch.reread)
                .map(|span| Enumerated {
                    oldest: key_ts(&span.lo),
                    latest: key_ts(&span.hi),
                    latest_included: top_included,
                });
            totals.pruned += self
                .db
                .store_history_page(channel_id, &rows, covered.as_ref(), enumerated.as_ref())
                .await?;
            totals.messages += messages.len();
            self.bar.did(messages.len() as u64);
            self.bar
                .doing(&format!("listing  msgs={}", totals.messages));

            if short {
                return Err(cut_short(M_HISTORY));
            }
            if last {
                return Ok(());
            }
            if let Some(oldest) = in_stretch.into_iter().min() {
                top = Some(oldest);
                top_included = false;
            }
        }
    }

    /// The threads of the channel owed by the store's own account, one
    /// `conversations.replies` walk each. A thread that fails costs that
    /// thread: an attempt on its own row, which is owed still.
    async fn fetch_owed_threads(&self, channel_id: &str, totals: &mut ChannelTotals) -> Result<()> {
        let listed = self.db.threads_listed(channel_id).await?;
        let owed = owed::owed(self.db.pool(), THREADS, listed).await?;
        let l = owed::Loop {
            pool: self.db.pool(),
            table: THREADS,
            phase: M_REPLIES,
            stop: self.stop,
            found: self.found,
            sealer: self.sealer,
            batch: 1,
            concurrency: 1,
            flush: 1,
            flush_bytes: 0,
            failures_in_a_row: 0,
        };
        let threads = Threads {
            mirror: self,
            channel_id,
            messages: totals.messages,
            replies: AtomicUsize::new(0),
            pruned: AtomicUsize::new(0),
        };
        owed::drain(&l, owed, &threads).await?;
        totals.replies += threads.replies.into_inner();
        totals.pruned += threads.pruned.into_inner();
        Ok(())
    }

    /// Every page of `conversations.replies` for the thread rooted at
    /// `key`: the thread whole, root copy included, or an error.
    async fn fetch_thread(
        &self,
        channel_id: &str,
        key: &str,
        replies: &AtomicUsize,
    ) -> Result<Thread> {
        let (_, _, thread_ts) =
            schema_raw::split_key(key).ok_or_else(|| anyhow::anyhow!("{key}: not a thread key"))?;
        let mut base = BTreeMap::new();
        base.insert("channel".to_string(), channel_id.to_string());
        base.insert("ts".to_string(), thread_ts.to_string());
        base.insert("limit".to_string(), "200".to_string());

        let mut rows: Vec<MessageInput> = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let mut p = base.clone();
            if let Some(c) = &cursor {
                p.insert("cursor".to_string(), c.clone());
            }
            let resp = call(M_REPLIES, &p, self.latchkey).await?;
            let msgs: Vec<Value> = resp
                .get("messages")
                .and_then(|v| v.as_array())
                .map(|a| a.to_vec())
                .unwrap_or_default();
            let page: Vec<MessageInput> = msgs
                .iter()
                .filter_map(|m| reply_message_input(self.team_id, channel_id, thread_ts, m))
                .collect();
            // Announced before it is counted, as a history page is.
            let on_page = page.iter().filter(|m| !m.is_thread_root).count();
            self.bar.expect(on_page as u64);
            self.bar.did(on_page as u64);
            replies.fetch_add(on_page, Ordering::Relaxed);
            rows.extend(page);

            let has_more = resp.get("has_more").and_then(|v| v.as_bool());
            cursor = next_cursor(&resp);
            if cursor.is_none() && has_more == Some(true) {
                return Err(cut_short(M_REPLIES));
            }
            if cursor.is_none() || has_more == Some(false) {
                return Ok(Thread { rows });
            }
        }
    }

    /// The channel's edges without bytes, [`FILE_BATCH`] to a request.
    /// Every one is owed, whatever its sidecar says: only a landed file
    /// is held, and it lands with its hash. A store an older build wrote
    /// stamps some failed fetches as fetched.
    async fn fetch_owed_files(&self, channel_id: &str, totals: &mut ChannelTotals) -> Result<()> {
        let owed = self.db.files_listed(self.team_id, channel_id).await?;
        self.bar.expect(owed.len() as u64);
        let l = owed::Loop {
            pool: self.db.pool(),
            table: SlackAttachmentRow::TABLE,
            phase: "files",
            stop: self.stop,
            found: self.found,
            sealer: self.sealer,
            batch: FILE_BATCH,
            concurrency: 1,
            flush: FILE_BATCH,
            flush_bytes: 0,
            failures_in_a_row: 0,
        };
        let files = FileFetcher {
            db: self.db,
            latchkey: self.latchkey,
            blob_size_limit_bytes: self.blob_size_limit_bytes,
            blake3_by_file: self.blake3_by_file,
            bar: self.bar,
            downloaded: AtomicUsize::new(0),
        };
        let drained = owed::drain(&l, owed, &files).await?;
        let downloaded = files.downloaded.into_inner();
        for (outcome, n) in [
            ("downloaded", downloaded),
            ("skipped", drained.got.saturating_sub(downloaded)),
            ("too_large", drained.skipped),
            ("error", drained.failed),
        ] {
            if n > 0 {
                *totals.media.entry(outcome.to_string()).or_insert(0) += n;
            }
        }
        self.bar.doing(&format!(
            "msgs={} replies={} media={}",
            totals.messages,
            totals.replies,
            totals.media.get("downloaded").copied().unwrap_or(0)
        ));
        Ok(())
    }
}

fn history_message_input(team_id: &str, channel_id: &str, m: &Value) -> Option<MessageInput> {
    let ts = m.get("ts").and_then(|v| v.as_str())?;
    let thread_ts = m
        .get("thread_ts")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let is_thread_root = match thread_ts.as_deref() {
        None => true,
        Some(tts) => tts == ts,
    };
    Some(MessageInput {
        team_id: team_id.to_string(),
        channel_id: channel_id.to_string(),
        ts: ts.to_string(),
        thread_ts,
        is_thread_root,
        user_id: m.get("user").and_then(|v| v.as_str()).map(String::from),
        payload: m.clone(),
    })
}

fn reply_message_input(
    team_id: &str,
    channel_id: &str,
    requested_thread_ts: &str,
    m: &Value,
) -> Option<MessageInput> {
    let ts = m.get("ts").and_then(|v| v.as_str())?;
    let thread_ts = m
        .get("thread_ts")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or_else(|| Some(requested_thread_ts.to_string()));
    let is_thread_root = ts == requested_thread_ts;
    Some(MessageInput {
        team_id: team_id.to_string(),
        channel_id: channel_id.to_string(),
        ts: ts.to_string(),
        thread_ts,
        is_thread_root,
        user_id: m.get("user").and_then(|v| v.as_str()).map(String::from),
        payload: m.clone(),
    })
}

// Public entry point.

pub struct FetchOptions {
    /// Which latchkey identity the download authenticates as, from the
    /// source's `latchkey_settings:` block.
    pub latchkey: LatchkeySettings,
    /// Seals what has been written so far, so render can start on the
    /// channels already walked while the rest are still arriving. `None`
    /// -- the default -- commits once at the end.
    pub sealer: Option<datalib_etl::raw_store::Sealer>,
    /// The store this run writes into, opened and closed by the caller.
    /// A download never opens a store of its own: one writer per file
    /// (`datalib/backend/etl/README.md` § "One writer per file, by
    /// construction").
    pub db: RawDb,
    pub channels: Option<Vec<String>>,
    pub since: String,
    pub refresh_window_days: i64,
    pub members_only: bool,
    pub media: bool,
    /// Mirror direct messages (1:1 and group). Off by default — see
    /// `SlackApiSync::dms`.
    pub dms: bool,
    /// Restrict DMs to these conversations — Slack ids or pasted links,
    /// see [`conversation_id`]. Only consulted when `dms` is on;
    /// `SlackApiSync::validate` rejects the other combination before it
    /// gets here.
    pub dm_conversations: Option<Vec<String>>,
    pub blob_size_limit_bytes: Option<u64>,
    /// The run's one "now": the refresh window and the age of a listing
    /// sweep are measured from it.
    pub now: DateTime<Utc>,
    pub progress: datalib_etl::progress::Progress,
    pub control: datalib_etl::control::DownloadControl,
}

impl FetchOptions {
    /// Every field defaulted except the store, which has none to give:
    /// it is a live handle the caller opens and closes.
    pub fn new(db: RawDb) -> Self {
        Self {
            db,
            sealer: None,
            latchkey: LatchkeySettings::default(),
            channels: None,
            since: DEFAULT_SINCE.to_string(),
            refresh_window_days: DEFAULT_REFRESH_WINDOW_DAYS,
            members_only: true,
            media: true,
            dms: false,
            dm_conversations: None,
            blob_size_limit_bytes: None,
            now: Utc::now(),
            progress: datalib_etl::progress::Progress::noop(),
            control: datalib_etl::control::DownloadControl::default(),
        }
    }
}

#[derive(serde::Serialize)]
pub struct FetchSummary {
    /// Configured `channels` / `dm_conversations` this workspace has
    /// nothing matching. Reported rather than fatal.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<DownloadProblem>,
    pub messages: usize,
    pub replies: usize,
    /// Messages Slack no longer serves inside a range this run re-walked.
    pub pruned: usize,
    pub media: BTreeMap<String, usize>,
    pub account: AccountTotals,
}

#[instrument(skip_all)]
pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let (pool, stop) = (opts.db.pool().clone(), opts.control.stop.clone());
    let sealer = opts.sealer.clone();
    run_problems::collecting_sealed(&pool, &stop, sealer.as_ref(), |found| download(opts, found))
        .await
}

async fn download(opts: FetchOptions, found: RunProblems) -> Result<FetchSummary> {
    let _ = datalib_etl_web::latchkey::ensure_curl_router();
    let db = opts.db.clone();

    let since_dt =
        parse_iso_or_utc_date(&opts.since).with_context(|| format!("--since {:?}", opts.since))?;
    let since = ts_key(&datetime_to_slack_ts(&since_dt));
    let now = opts.now;
    let refresh_from = (opts.refresh_window_days > 0).then(|| {
        ts_key(&datetime_to_slack_ts(&days_before(
            now,
            opts.refresh_window_days,
        )))
    });

    let run_config = json!({
        "channels": opts.channels,
        "since": opts.since,
        "refresh_window_days": opts.refresh_window_days,
        "members_only": opts.members_only,
        "media": opts.media,
        "dms": opts.dms,
        "dm_conversations": opts.dm_conversations,
        "blob_size_limit_bytes": opts.blob_size_limit_bytes,
    });
    let run = datalib_etl::download_run::DownloadRun::start(db.pool(), &run_config).await?;

    let blake3_by_file = Mutex::new(db.load_attachment_blake3s().await?);

    let mut grand = FetchSummary {
        problems: Vec::new(),
        messages: 0,
        replies: 0,
        pruned: 0,
        media: BTreeMap::new(),
        account: AccountTotals::default(),
    };
    let work = async {
        // The step's own handle: setup only names what it is doing.
        let setup = opts.progress.clone();
        setup.set_message("starting");
        let t_setup = std::time::Instant::now();
        // Without the workspace's identity nothing below can be keyed:
        // the one failure that fails the run.
        let (team_id, self_user_id) = fetch_self(&db, &setup, &opts.latchkey).await?;
        // Users before channels: a DM is titled after its counterpart,
        // so the DM progress labels need the user directory to already
        // be mirrored. A listing that fails leaves the stored one.
        if let Err(e) = fetch_users(&db, &now, &setup, &opts.latchkey).await {
            found.push(listing_problem(M_USERS, &e));
        }
        let listed = match fetch_channels(
            &db,
            opts.members_only,
            opts.channels.is_some(),
            opts.dms,
            &now,
            &setup,
            &opts.latchkey,
        )
        .await
        {
            Ok(listed) => listed,
            // The channels an earlier listing stored are still worth
            // walking; with none stored there is nothing to do at all.
            Err(e) => {
                let stored = db
                    .channels_for_fetch(opts.members_only, opts.channels.is_some(), opts.dms)
                    .await?;
                if stored.is_empty() {
                    return Err(e.context("no channels are stored from an earlier listing"));
                }
                found.push(listing_problem(M_CHANNELS, &e));
                stored
            }
        };
        // Only loaded when DMs are in play — it names them, and for a
        // channels-only run it is a whole table scan nothing would read.
        let user_labels: BTreeMap<String, String> = if opts.dms {
            db.user_directory()
                .await?
                .iter()
                .map(|u| (u.id.clone(), u.label()))
                .collect()
        } else {
            BTreeMap::new()
        };

        let plan = select_targets(
            &listed,
            opts.channels.as_deref(),
            opts.dm_conversations.as_deref(),
            &user_labels,
            self_user_id.as_deref(),
        );
        for spec in &plan.unmatched {
            grand.problems.push(DownloadProblem::not_found(
                "channels",
                spec,
                format!("no channel by that name among {} listed", listed.len()),
            ));
        }
        for spec in &plan.unmatched_dms {
            grand.problems.push(DownloadProblem::not_found(
                "dm_conversations",
                spec,
                "no direct message with that id among the ones this account can see, so it is \
                 not mirrored",
            ));
        }
        found.config(grand.problems.clone());
        info!(
            event = "slack_export_planned",
            channels = plan.targets.len() - plan.dm_targets,
            dms = opts.dms,
            dm_targets = plan.dm_targets,
            media = opts.media,
            "planned the export"
        );
        let targets = plan.targets;

        if !opts.control.stop.requested() {
            let (account, problems) = fetch_account_state(
                &db,
                &targets,
                &now,
                &opts.control.stop,
                &setup,
                &opts.latchkey,
            )
            .await?;
            found.extend(problems);
            if let Some(sealer) = opts.sealer.as_ref() {
                sealer
                    .wrote(
                        (account.read_states
                            + account.saved_items
                            + account.bookmarks
                            + account.pruned) as u64,
                    )
                    .await;
            }
            grand.account = account;
        }
        setup.finish(&format!(
            "setup done in {}ms",
            t_setup.elapsed().as_millis() as u64
        ));

        // Seeded with one tick per channel, so the bar reads as
        // something before the first channel has listed anything.
        let bar = RunBar::new(&opts.progress, targets.len() as u64);
        let mirror = Mirror {
            db: &db,
            team_id: &team_id,
            since,
            refresh_from,
            media: opts.media,
            blob_size_limit_bytes: opts.blob_size_limit_bytes,
            bar: &bar,
            latchkey: &opts.latchkey,
            stop: &opts.control.stop,
            found: &found,
            sealer: opts.sealer.as_ref(),
            blake3_by_file: &blake3_by_file,
        };
        for (cid, name) in &targets {
            // Asked to stop: end here rather than start a channel whose
            // first request the transport would refuse.
            if opts.control.stop.requested() {
                info!(event = "slack_interrupted", next_channel = %name, "told to stop; leaving the rest for the next run");
                break;
            }
            bar.doing(&format!("{name}: listing"));
            let span = info_span!("channel", channel_name = %name, channel_id = %cid);
            let mut totals = ChannelTotals::default();
            let result = mirror.channel(cid, &mut totals).instrument(span).await;
            info!(
                event = "slack_channel_done",
                channel = %name,
                messages = totals.messages,
                replies = totals.replies,
                media = totals.media.get("downloaded").copied().unwrap_or(0),
                "finished one channel",
            );
            bar.did(1);
            // A channel that failed costs only itself: what it did not
            // cover or fetch is still owed, by the store's own account.
            if let Err(e) = result {
                found.push(listing_problem(&format!("{M_HISTORY} {name}"), &e));
            }
            let written = (totals.messages + totals.replies + totals.pruned) as u64;
            grand.messages += totals.messages;
            grand.replies += totals.replies;
            grand.pruned += totals.pruned;
            for (k, v) in totals.media {
                *grand.media.entry(k).or_insert(0) += v;
            }
            if let Some(sealer) = opts.sealer.as_ref() {
                sealer.wrote(written).await;
            }
        }
        bar.finish();
        Ok::<(), anyhow::Error>(())
    };

    let result = work.await;
    run.finish(&result, &grand).await;
    result?;

    info!(
        event = "slack_export_complete",
        messages = grand.messages,
        replies = grand.replies,
        "the export is done"
    );
    Ok(grand)
}

fn parse_iso_or_utc_date(s: &str) -> Result<DateTime<Utc>> {
    let t = datalib_time::parse_strict(s)
        .or_else(|_| datalib_time::parse_yyyy_mm_dd_assumed_utc(s))
        .with_context(|| format!("expected RFC 3339 or YYYY-MM-DD, got {s:?}"))?;
    Ok(t.inner().with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SINCE: &str = "1704067200.000000";

    fn span(lo: &str, hi: &str) -> Span {
        Span::new(ts_key(lo), ts_key(hi))
    }

    fn gap(lo: &str, hi: &str, inclusive: bool) -> Stretch {
        Stretch {
            span: span(lo, hi),
            inclusive,
            reread: false,
        }
    }

    fn planned(refresh_from: Option<&str>, held: &[Span]) -> Vec<Stretch> {
        stretches(&ts_key(SINCE), refresh_from.map(ts_key).as_deref(), held)
    }

    /// The TNG fixtures' stardate `ts`es have an eleventh digit, which
    /// as plain strings sort below every real one.
    #[test]
    fn a_ts_key_sorts_by_instant_whatever_the_width_and_reads_back() {
        assert!(ts_key("12604000800.000800") > ts_key(SINCE));
        assert!(ts_key("999999999.000000") < ts_key(SINCE));
        assert!(ts_key("12604000800.000800").as_str() < END_OF_TIME);
        assert_eq!(key_ts(&ts_key(SINCE)), SINCE);
        assert_eq!(key_ts(&ts_key("0.000001")), "0.000001");
    }

    /// #1048: `since = "0001-01-01"` went out as
    /// `oldest=-62135596800.000000`, which Slack answers with an empty
    /// page, so the mirror held no messages.
    #[test]
    fn a_since_before_the_epoch_asks_from_the_epoch() {
        for since in ["0001-01-01", "1969-12-31T23:59:59.5Z"] {
            let ts = datetime_to_slack_ts(&parse_iso_or_utc_date(since).unwrap());
            assert_eq!(ts, "0.000000", "{since}");
            assert_eq!(key_ts(&ts_key(&ts)), "0.000000", "{since}");
        }
        let later = parse_iso_or_utc_date("1970-01-02").unwrap();
        assert_eq!(datetime_to_slack_ts(&later), "86400.000000");
    }

    /// A refresh window longer than the calendar reaches panicked in
    /// the subtraction; it is the whole history.
    #[test]
    fn a_refresh_window_past_the_start_of_time_reaches_the_epoch() {
        let now = parse_iso_or_utc_date("2369-04-01").unwrap();
        for days in [36_500_000, 1_000_000_000, i64::MAX] {
            assert_eq!(
                datetime_to_slack_ts(&days_before(now, days)),
                "0.000000",
                "{days}"
            );
        }
        assert_eq!(
            days_before(now, 30),
            parse_iso_or_utc_date("2369-03-02").unwrap()
        );
    }

    #[test]
    fn a_channel_never_walked_is_one_open_stretch_from_since() {
        assert_eq!(planned(None, &[]), [gap(SINCE, END_OF_TIME, true)]);
    }

    /// What is newer than the newest message seen is asked for on every
    /// run, without asking for that message again.
    #[test]
    fn a_channel_walked_whole_owes_only_what_is_newer() {
        let held = [span(SINCE, "1735689600.000300")];
        assert_eq!(
            planned(None, &held),
            [gap("1735689600.000300", END_OF_TIME, false)]
        );
    }

    /// History is newest first, so a walk that stored its first page and
    /// died has covered the top. The store's newest message says nothing
    /// about what is under that page.
    #[test]
    fn a_walk_cut_off_after_its_first_page_owes_what_is_under_it() {
        let held = [span("1735689600.000200", "1735689600.000300")];
        assert_eq!(
            planned(None, &held),
            [
                gap("1735689600.000300", END_OF_TIME, false),
                gap(SINCE, "1735689600.000200", true),
            ]
        );
    }

    #[test]
    fn a_widened_since_owes_the_stretch_below_the_old_one() {
        let held = [span("1735689600.000000", "1735689600.000300")];
        assert_eq!(
            planned(None, &held)[1],
            gap(SINCE, "1735689600.000000", true)
        );
    }

    /// The window re-reads what was covered and nothing else: a stretch
    /// never read is a gap, walked once.
    #[test]
    fn the_refresh_rereads_what_was_already_covered_from_the_window_up() {
        let reread = |lo: &str, hi: &str| Stretch {
            span: span(lo, hi),
            inclusive: true,
            reread: true,
        };
        let held = [span(SINCE, "1735689600.000300")];
        assert_eq!(
            planned(Some("1735000000.000000"), &held),
            [
                gap("1735689600.000300", END_OF_TIME, false),
                reread("1735000000.000000", "1735689600.000300"),
            ]
        );
        // A window reaching below `since` stops at it, and one above
        // everything covered re-reads nothing.
        assert_eq!(
            planned(Some("1000000000.000000"), &held)[1],
            reread(SINCE, "1735689600.000300")
        );
        assert_eq!(planned(Some("1736000000.000000"), &held).len(), 1);
    }

    /// Three pages of a cold walk: the first settles from its oldest
    /// message up to its newest, each next one down to its own oldest,
    /// the last down to `since`.
    #[test]
    fn each_page_settles_from_its_oldest_message_up_to_the_page_before() {
        let cold = span(SINCE, END_OF_TIME);
        let keys = |ts: &[&str]| ts.iter().map(|t| ts_key(t)).collect::<Vec<_>>();
        assert_eq!(
            settled(
                &cold,
                None,
                &keys(&["1735689600.000300", "1735689600.000200"]),
                false
            ),
            Some(span("1735689600.000200", "1735689600.000300"))
        );
        let top = ts_key("1735689600.000200");
        assert_eq!(
            settled(&cold, Some(&top), &keys(&["1735689600.000100"]), false),
            Some(span("1735689600.000100", "1735689600.000200"))
        );
        let top = ts_key("1735689600.000100");
        assert_eq!(
            settled(&cold, Some(&top), &keys(&["1735689600.000000"]), true),
            Some(span(SINCE, "1735689600.000100"))
        );
    }

    /// An open-topped walk that found nothing has looked at no stretch
    /// it can name, so the channel is asked again next run.
    #[test]
    fn an_empty_walk_of_the_open_top_settles_nothing() {
        assert_eq!(settled(&span(SINCE, END_OF_TIME), None, &[], true), None);
        // A closed stretch that came back empty was read whole.
        let below = span(SINCE, "1735689600.000100");
        let top = ts_key("1735689600.000100");
        assert_eq!(settled(&below, Some(&top), &[], true), Some(below.clone()));
    }

    // ── DM scoping ───────────────────────────────────────────────────

    /// The enforcement point for `dms = false` is the request, not a
    /// local filter: without `im,mpim` Slack never returns a DM at all.
    #[test]
    fn dm_types_are_requested_only_when_dms_are_on() {
        assert_eq!(
            conversation_types(false),
            "public_channel,private_channel",
            "a DM must not even be listed when dms is off"
        );
        assert_eq!(
            conversation_types(true),
            "public_channel,private_channel,im,mpim"
        );
    }

    fn user(id: &str, name: &str, real: Option<&str>, display: Option<&str>) -> UserDirectoryEntry {
        UserDirectoryEntry {
            id: id.into(),
            name: Some(name.into()),
            real_name: real.map(String::from),
            display_name: display.map(String::from),
        }
    }

    fn directory() -> Vec<UserDirectoryEntry> {
        vec![
            user("U1", "picard", Some("Jean-Luc Picard"), Some("Captain")),
            user("U2", "riker", Some("William Riker"), Some("Number One")),
            user("U3", "data", None, None),
        ]
    }

    /// Every way a person gets a conversation id out of Slack: the id
    /// itself, `Copy link` on the conversation, the app's address bar,
    /// and a link to one message inside it.
    #[test]
    fn conversation_id_reads_bare_ids_and_pasted_links() {
        for spec in [
            "D0123ABCD",
            "  D0123ABCD ",
            "https://enterprise.slack.com/archives/D0123ABCD",
            "https://enterprise.slack.com/archives/D0123ABCD/",
            "https://enterprise.slack.com/archives/D0123ABCD/p1735689600000000?thread_ts=1",
            "https://app.slack.com/client/T0NCC1701/D0123ABCD",
        ] {
            assert_eq!(conversation_id(spec), "D0123ABCD", "{spec:?}");
        }
        assert_eq!(
            conversation_id("https://app.slack.com/client/T0NCC1701/G0123ABCD"),
            "G0123ABCD"
        );
    }

    /// Something that is neither comes back as typed, so the "not
    /// found" report says what the person wrote.
    #[test]
    fn conversation_id_leaves_the_unparseable_alone() {
        assert_eq!(conversation_id("@riker"), "@riker");
        assert_eq!(
            conversation_id("https://slack.com/help"),
            "https://slack.com/help"
        );
    }

    fn channel_target(id: &str, name: &str) -> FetchTarget {
        FetchTarget {
            id: id.into(),
            name: Some(name.into()),
            is_dm: false,
            dm_user_ids: Vec::new(),
        }
    }

    fn im(id: &str, user_id: &str) -> FetchTarget {
        FetchTarget {
            id: id.into(),
            name: None,
            is_dm: true,
            dm_user_ids: vec![user_id.into()],
        }
    }

    fn mpim(id: &str, name: &str, members: &[&str]) -> FetchTarget {
        FetchTarget {
            id: id.into(),
            name: Some(name.into()),
            is_dm: true,
            dm_user_ids: members.iter().map(|s| s.to_string()).collect(),
        }
    }

    fn labels() -> BTreeMap<String, String> {
        directory()
            .into_iter()
            .map(|u| (u.id.clone(), u.label()))
            .collect()
    }

    /// The account doing the mirroring — U1, Picard.
    const SELF: Option<&str> = Some("U1");

    fn listed() -> Vec<FetchTarget> {
        vec![
            channel_target("C1", "general"),
            im("D1", "U2"),
            im("D3", "U3"),
            mpim("G1", "mpdm-picard--riker--data-1", &["U1", "U2", "U3"]),
        ]
    }

    fn walked(plan: &TargetPlan) -> Vec<&str> {
        plan.targets.iter().map(|(id, _)| id.as_str()).collect()
    }

    /// The point of the separate namespace: `channels` scopes channels
    /// and nothing else. Running the channel-name filter over DMs — the
    /// natural mistake if the two shared one list — drops every DM,
    /// because a DM has no name to match.
    #[test]
    fn channels_filter_does_not_touch_dms() {
        let all = listed();
        let names = vec!["general".to_string()];
        let plan = select_targets(&all, Some(&names), None, &labels(), SELF);
        assert_eq!(walked(&plan), vec!["C1", "D1", "D3", "G1"]);
        assert_eq!(plan.dm_targets, 3);
    }

    #[test]
    fn no_allowlist_walks_every_dm_including_group_dms() {
        let plan = select_targets(&listed(), None, None, &labels(), SELF);
        assert_eq!(walked(&plan), vec!["C1", "D1", "D3", "G1"]);
    }

    fn specs(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|s| s.to_string()).collect()
    }

    /// Naming conversations names exactly those, 1:1 or group, in the
    /// order written — and a pasted link counts as its id.
    #[test]
    fn named_dm_conversations_are_walked_and_nothing_else() {
        let want = specs(&["G1", "https://enterprise.slack.com/archives/D1"]);
        let plan = select_targets(&listed(), None, Some(&want), &labels(), SELF);
        assert_eq!(walked(&plan), vec!["C1", "G1", "D1"]);
        assert_eq!(plan.dm_targets, 2);
        assert!(plan.unmatched_dms.is_empty());
    }

    /// A list that names nothing this account has must mirror no DMs —
    /// not fall open to all of them — and must say what it missed, in
    /// the person's own words.
    #[test]
    fn dm_conversations_matching_nothing_walk_no_dms() {
        let want = specs(&["D404", "@riker"]);
        let plan = select_targets(&listed(), None, Some(&want), &labels(), SELF);
        assert_eq!(walked(&plan), vec!["C1"]);
        assert_eq!(plan.dm_targets, 0);
        assert_eq!(plan.unmatched_dms, specs(&["D404", "@riker"]));
    }

    /// A channel id in `dm_conversations` is not a DM, however real the
    /// channel — the two lists stay separate namespaces.
    #[test]
    fn a_channel_id_is_not_a_dm_conversation() {
        let want = specs(&["C1"]);
        let plan = select_targets(&listed(), None, Some(&want), &labels(), SELF);
        assert_eq!(walked(&plan), vec!["C1"]);
        assert_eq!(plan.dm_targets, 0);
        assert_eq!(plan.unmatched_dms, specs(&["C1"]));
    }

    #[test]
    fn dm_labels_read_as_people() {
        let plan = select_targets(&listed(), None, None, &labels(), SELF);
        let by_id: BTreeMap<&str, &str> = plan
            .targets
            .iter()
            .map(|(id, label)| (id.as_str(), label.as_str()))
            .collect();
        assert_eq!(by_id["C1"], "general");
        assert_eq!(by_id["D1"], "@William Riker");
        // The account itself is subtracted, so a group DM reads as who
        // you are talking *to* — not `@…, Jean-Luc Picard, …`.
        // U3 has no real_name, so the label falls back to the handle —
        // the same rule the renderer uses.
        assert_eq!(by_id["G1"], "@William Riker, data");
    }

    /// A DM with someone `users.list` didn't return still gets walked —
    /// an unknown counterpart is a labelling problem, not a reason to
    /// drop their messages.
    #[test]
    fn dm_with_an_unknown_user_falls_back_to_the_id() {
        let all = vec![im("D9", "U404")];
        let plan = select_targets(&all, None, None, &labels(), SELF);
        assert_eq!(plan.targets, vec![("D9".to_string(), "@U404".to_string())]);
    }

    /// A store written before `dm_user_ids` existed has DM rows with no
    /// participants. They must still be walked and still be nameable —
    /// Slack's own composite handle, then the raw id.
    #[test]
    fn dm_without_stored_participants_still_gets_a_label() {
        let legacy_group = mpim("G9", "mpdm-riker--data-1", &[]);
        let nameless = FetchTarget {
            id: "D9".into(),
            name: None,
            is_dm: true,
            dm_user_ids: Vec::new(),
        };
        let plan = select_targets(&[legacy_group, nameless], None, None, &labels(), SELF);
        assert_eq!(
            plan.targets,
            vec![
                ("G9".to_string(), "@mpdm-riker--data-1".to_string()),
                ("D9".to_string(), "D9".to_string()),
            ]
        );
    }

    /// `dm_counterparts` is what reconciles the two wire shapes: an
    /// `im`'s `user` excludes you, an `mpim`'s `members` includes you.
    #[test]
    fn counterparts_subtract_self_but_never_to_nothing() {
        let members = vec!["U1".to_string(), "U2".to_string()];
        assert_eq!(
            schema_raw::dm_counterparts(&members, Some("U1")),
            vec!["U2".to_string()]
        );
        // A DM with yourself: subtracting would leave an unnameable
        // conversation, so the full list stands.
        let just_me = vec!["U1".to_string()];
        assert_eq!(
            schema_raw::dm_counterparts(&just_me, Some("U1")),
            vec!["U1".to_string()]
        );
        // `auth.test` without a `user_id` must not drop anyone.
        assert_eq!(schema_raw::dm_counterparts(&members, None), members);
    }
}
