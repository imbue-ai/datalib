//! Slack downloader entry point.

pub mod api;
pub mod db;
pub mod files;
mod frontier;
mod involving;
mod reply_search;
pub mod schema_raw;
mod search;
pub mod shapes;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Instant;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use futures::stream::{FuturesOrdered, StreamExt};
use serde_json::{json, Value};
use sqlx::{Sqlite, Transaction};
use tracing::{info, info_span, instrument, warn, Instrument};

use api::{call_slack, SlackCall, SlackError};
use async_trait::async_trait;
use datalib_etl::bulk::BulkUpsertable;
use datalib_etl::download_problems::{DownloadProblem, RunProblem, SilentEntry};
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
use files::{File, FileFetcher, FILE_BATCH};
use frontier::{Frontier, Next, Walk};
use involving::{InvolvingSearch, InvolvingTotals, Query};
use reply_search::{ReplySearch, ReplySearchTotals, SEARCH_LAG};
use schema_raw::{SlackAttachmentRow, UserRow, THREADS};
use shapes::{
    M_AUTH_TEST, M_BOOKMARKS, M_CHANNELS, M_COUNTS, M_HISTORY, M_REPLIES, M_SAVED, M_SEARCH,
    M_USERS, M_USER_INFO,
};

pub const DEFAULT_SINCE: &str = "2024-01-01";
pub const DEFAULT_REFRESH_WINDOW_DAYS: i64 = 30;
/// How long after a conversation's refresh window was last re-read
/// whole before a run reads it again.
pub const DEFAULT_REFRESH_EVERY_MINUTES: i64 = 60;

/// Max age of a successful user or bookmark sweep before we refetch.
/// A new poster does not wait for it: the messages name their authors,
/// and an author with no `users` row is looked up on its own.
pub const MANIFEST_TTL: chrono::Duration = chrono::Duration::hours(6);

/// On a real workspace, 381 open channels took 5 calls at 1000 and 23
/// at 200; listed with its 469 archived ones, 850 took 5 at 1000.
const CHANNELS_PER_PAGE: &str = "1000";

/// What `conversations.history` and `conversations.replies` are asked
/// for per page: Slack's most. It may answer fewer, so a page's size
/// is never read as the end of anything; the cursor is.
pub const MESSAGES_PER_PAGE: &str = "999";

/// Requests the walk has out at once, within the source's own limit.
/// Three is what was measured to read history 5.5 times as fast with no
/// request rate-limited (INGEST.md § "Three requests at once").
const AT_ONCE: usize = 3;

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

#[instrument(skip(db, progress, latchkey))]
async fn fetch_channels(
    db: &RawDb,
    members_only: bool,
    include_archived: bool,
    dms: bool,
    progress: &datalib_etl::progress::Progress,
    latchkey: &LatchkeySettings,
) -> Result<Vec<FetchTarget>> {
    // Archived channels are listed whatever the config says, so each
    // stored `is_archived` follows Slack's: a listing that left them out
    // would never say a mirrored channel had been archived.
    // `channels_for_fetch` leaves them out of the walk unless asked.
    let mut params = BTreeMap::new();
    params.insert("exclude_archived".to_string(), "false".to_string());
    params.insert("limit".to_string(), CHANNELS_PER_PAGE.to_string());
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

/// `client.counts` has one entry per conversation open in the account's
/// sidebar, mirrored or not; only the mirrored ones are kept, so
/// `dms = false` keeps DMs out of this table as it does out of
/// `channels`. Returns the entries kept.
async fn fetch_read_states(
    db: &RawDb,
    in_scope: &HashSet<String>,
    counts: Option<Result<Value>>,
    latchkey: &LatchkeySettings,
) -> Result<Vec<Value>> {
    let resp = match counts {
        Some(counts) => counts?,
        None => call(M_COUNTS, &empty_params(), latchkey).await?,
    };
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
    Ok(entries)
}

/// The newest message `client.counts` names for each of `entries`, by
/// conversation id. Slack lists only the conversations open in the
/// sidebar, so a conversation missing here is closed, not empty.
fn open_latest(entries: &[Value]) -> HashMap<String, String> {
    entries
        .iter()
        .filter_map(|e| {
            let id = e.get("id").and_then(Value::as_str)?;
            let latest = e.get("latest").and_then(Value::as_str)?;
            Some((id.to_string(), latest.to_string()))
        })
        .collect()
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

/// What [`fetch_account_state`] found.
struct AccountState {
    totals: AccountTotals,
    problems: Vec<RunProblem>,
    /// [`open_latest`] of the read states; `None` when `client.counts`
    /// could not be read.
    open_latest: Option<HashMap<String, String>>,
}

/// Read states, saved items and bookmarks for the conversations this run
/// mirrors. Each listing that fails becomes a `problems` row and costs
/// only its own table; none of them sinks the message walk. `counts` is
/// a `client.counts` answer the run already asked for, if it did.
async fn fetch_account_state(
    db: &RawDb,
    targets: &[(String, String)],
    counts: Option<Result<Value>>,
    now: &DateTime<Utc>,
    stop: &datalib_etl::stop::StopFlag,
    progress: &datalib_etl::progress::Progress,
    latchkey: &LatchkeySettings,
) -> Result<AccountState> {
    let in_scope: HashSet<String> = targets.iter().map(|(cid, _)| cid.clone()).collect();
    let mut totals = AccountTotals::default();
    let mut problems = Vec::new();

    // Three calls that need nothing from each other, out together.
    let mut bookmark_problems = Vec::new();
    let (read_states, saved, bookmarks) = tokio::join!(
        async {
            progress.set_message(M_COUNTS);
            fetch_read_states(db, &in_scope, counts, latchkey).await
        },
        list_saved_items(latchkey),
        fetch_bookmarks(
            db,
            targets,
            now,
            stop,
            progress,
            latchkey,
            &mut bookmark_problems
        ),
    );
    let open_latest = match read_states {
        Ok(entries) => {
            totals.read_states = entries.len();
            Some(self::open_latest(&entries))
        }
        Err(e) => {
            problems.push(listing_problem(M_COUNTS, &e));
            None
        }
    };

    match saved {
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

    let (bookmarks, pruned) = bookmarks?;
    problems.extend(bookmark_problems);
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
    Ok(AccountState {
        totals,
        problems,
        open_latest,
    })
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

/// The stretch every channel wants when nothing narrows it: everything
/// from `since` up.
fn from_since(since: &str) -> Vec<Span> {
    vec![Span::new(since, END_OF_TIME)]
}

/// What to walk of a channel that has `held` covered: the gaps in each
/// `wanted` stretch, newest first, then whatever of them was already
/// covered from `refresh_from` up. A gap's ends are read when they are
/// the ends of what is wanted, and not when they are the ends of what is
/// held.
fn stretches(wanted: &[Span], refresh_from: Option<&str>, held: &[Span]) -> Vec<Stretch> {
    let mut out: Vec<Stretch> = Vec::new();
    for want in wanted.iter().rev() {
        for gap in coverage::gaps(want, held).into_iter().rev() {
            out.push(Stretch {
                inclusive: gap.lo == want.lo || (gap.hi == want.hi && want.hi != END_OF_TIME),
                span: gap,
                reread: false,
            });
        }
    }
    let Some(refresh_from) = refresh_from else {
        return out;
    };
    let held = coverage::merged(held.to_vec());
    for want in wanted.iter().rev() {
        let from = refresh_from.max(want.lo.as_str());
        for span in held.iter().rev() {
            let hi = span.hi.as_str().min(want.hi.as_str());
            let lo = span.lo.as_str().max(from);
            if span.hi.as_str() > from && lo <= hi {
                out.push(Stretch {
                    span: Span::new(lo, hi),
                    inclusive: true,
                    reread: true,
                });
            }
        }
    }
    out
}

/// What `client.counts` says of a conversation's newest message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Newest<'a> {
    /// Nothing: `client.counts` failed, or the conversation is a channel
    /// it does not list.
    Unknown,
    /// A DM `client.counts` leaves out, being closed in the sidebar.
    /// `check_due` when it has not been walked through to its newest
    /// message within [`CLOSED_DM_CHECK_EVERY`].
    Closed {
        check_due: bool,
    },
    /// An archived channel, which takes no new messages. `read` once a
    /// walk that reached its newest message has finished since Slack
    /// last listed it open.
    Archived {
        read: bool,
    },
    At(&'a str),
}

/// How often a DM closed in the sidebar is walked for what is newer than
/// what is held. Slack reopens a DM when a message arrives, which
/// `client.counts` shows; this catches one closed again before a run saw
/// it open.
const CLOSED_DM_CHECK_EVERY: chrono::Duration = chrono::Duration::days(1);

fn closed_dm_check_key(channel_id: &str) -> String {
    format!("closed_dm:{channel_id}")
}

fn archived_read_key(channel_id: &str) -> String {
    format!("archived:{channel_id}")
}

/// Whether a stretch a conversation `owes` reaches down to its newest
/// message, `latest`.
fn shows_activity(latest: &str, owes: &[Stretch]) -> bool {
    let latest = ts_key(latest);
    owes.iter()
        .filter(|s| !s.reread)
        .any(|s| s.span.lo < latest || (s.inclusive && s.span.lo == latest))
}

/// What a run walks of one conversation: its [`stretches`], less the
/// ones `client.counts` shows hold nothing new. A conversation whose
/// newest message is already covered costs no request, except its
/// re-read when `refresh_from` says one is due. A closed DM is walked
/// whole when its check is due, and otherwise only for a gap under
/// what is held, which a `since` moved earlier leaves. An archived
/// channel is walked whole once, and after that only for such a gap:
/// nothing in it is new or edited.
fn to_walk(
    wanted: &[Span],
    refresh_from: Option<&str>,
    held: &[Span],
    newest: Newest,
) -> Vec<Stretch> {
    let all = stretches(wanted, refresh_from, held);
    match newest {
        Newest::Unknown | Newest::Closed { check_due: true } | Newest::Archived { read: false } => {
            all
        }
        Newest::Closed { check_due: false } | Newest::Archived { read: true } => all
            .into_iter()
            .filter(|s| !s.reread && s.span.hi != END_OF_TIME)
            .collect(),
        Newest::At(latest) if shows_activity(latest, &all) => all,
        Newest::At(_) => all.into_iter().filter(|s| s.reread).collect(),
    }
}

/// Whether a conversation's refresh window is due a re-read, `age`
/// after the last one finished (`None`: never).
fn refresh_due(age: Option<chrono::Duration>, every: chrono::Duration) -> bool {
    every <= chrono::Duration::zero() || age.is_none_or(|age| age >= every)
}

fn refresh_sweep_key(channel_id: &str) -> String {
    format!("refresh:{channel_id}")
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

/// What a run has fetched so far.
#[derive(Default)]
struct Totals {
    messages: usize,
    replies: usize,
    /// Messages and replies Slack has stopped serving inside a range we
    /// read whole.
    pruned: usize,
    media: BTreeMap<String, usize>,
}

impl Totals {
    fn written(&self) -> usize {
        self.messages + self.replies + self.pruned
    }

    fn line(&self) -> String {
        format!(
            "msgs={} replies={} media={}",
            self.messages,
            self.replies,
            self.media.get("downloaded").copied().unwrap_or(0)
        )
    }
}

/// What the run's fetches share.
struct Mirror<'a> {
    db: &'a RawDb,
    team_id: &'a str,
    blob_size_limit_bytes: Option<u64>,
    bar: &'a RunBar,
    latchkey: &'a LatchkeySettings,
    stop: &'a StopFlag,
    found: &'a RunProblems,
    sealer: Option<&'a Sealer>,
    blake3_by_file: &'a Mutex<HashMap<String, String>>,
}

/// One page of history, stored.
struct Page {
    rows: Vec<MessageInput>,
    pruned: usize,
    /// The walk is over: this page reached the bottom of its stretch.
    last: bool,
}

/// Owed threads, for [`owed`]: a thread is one record whose fetch is
/// paged.
struct Threads<'a> {
    mirror: &'a Mirror<'a>,
    /// Replies stored.
    replies: AtomicUsize,
    pruned: AtomicUsize,
    /// The stored replies that carry files, whose bytes are owed next.
    with_files: Mutex<Vec<MessageInput>>,
}

#[async_trait]
impl Fetcher<Thread> for Threads<'_> {
    async fn fetch(
        &self,
        batch: Vec<Listed>,
    ) -> std::result::Result<Vec<Fetched<Thread>>, BatchError> {
        let mut out = Vec::with_capacity(batch.len());
        for listed in batch {
            let outcome = match self.mirror.fetch_thread(&listed.key).await {
                Ok(thread) => Outcome::Got(thread),
                // The loop reads a stop off its flag; every other error
                // is this thread's.
                Err(e) if interrupted(&e) => return Err(BatchError::Batch(e)),
                Err(e) => Outcome::Failed(format!("{e:#}")),
            };
            out.push(Fetched { listed, outcome });
            self.mirror.bar.did(1);
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
        let mut with_files = self.with_files.lock().expect("not poisoned");
        for f in batch {
            if let Outcome::Got(thread) = &f.outcome {
                let replies = thread.rows.iter().filter(|m| !m.is_thread_root).count();
                self.replies.fetch_add(replies, Ordering::Relaxed);
                with_files.extend(
                    thread
                        .rows
                        .iter()
                        .filter(|m| m.payload.get("files").is_some())
                        .cloned(),
                );
            }
        }
        Ok(())
    }
}

/// Owed users, for [`owed::drain`]: one `users.info` each.
struct UserLookup<'a> {
    db: &'a RawDb,
    latchkey: &'a LatchkeySettings,
}

/// `users.info`'s user as `users.list` lists it, so a later listing of
/// the same person is no change: `users.info` adds the custom profile
/// fields.
fn as_listed(mut user: Value) -> Value {
    if let Some(profile) = user.get_mut("profile").and_then(Value::as_object_mut) {
        profile.remove("fields");
    }
    user
}

#[async_trait]
impl Fetcher<Value> for UserLookup<'_> {
    async fn fetch(
        &self,
        batch: Vec<Listed>,
    ) -> std::result::Result<Vec<Fetched<Value>>, BatchError> {
        let mut out = Vec::with_capacity(batch.len());
        for listed in batch {
            let mut p = BTreeMap::new();
            p.insert("user".to_string(), listed.key.clone());
            let user = call(M_USER_INFO, &p, self.latchkey).await.and_then(|resp| {
                resp.get("user")
                    .cloned()
                    .ok_or_else(|| anyhow::anyhow!("{M_USER_INFO} {}: no `user`", listed.key))
            });
            let outcome = match user {
                Ok(user) => Outcome::Got(as_listed(user)),
                Err(e) if interrupted(&e) => return Err(BatchError::Batch(e)),
                // A token that may not look users up refuses every one.
                Err(e) if refused(&e) => return Err(BatchError::Terminal(e)),
                Err(e) => Outcome::Failed(format!("{e:#}")),
            };
            out.push(Fetched { listed, outcome });
        }
        Ok(out)
    }

    async fn store(
        &self,
        tx: &mut Transaction<'static, Sqlite>,
        batch: &[Fetched<Value>],
    ) -> Result<()> {
        self.db.store_looked_up_users(tx, batch).await
    }
}

/// Users looked up per transaction.
const USER_FLUSH: usize = 50;

impl Mirror<'_> {
    /// Every user [`RawDb::users_owed`] names, one `users.info` each. A
    /// lookup that fails is an attempt on the user's own row, owed still.
    async fn fetch_users_owed(&self) -> Result<owed::Drained> {
        self.db.settle_listed_users().await?;
        let owed = self.db.users_owed().await?;
        if owed.is_empty() {
            return Ok(owed::Drained::default());
        }
        self.bar
            .doing(&format!("{M_USER_INFO}: {} new authors", owed.len()));
        let l = owed::Loop {
            pool: self.db.pool(),
            table: UserRow::TABLE,
            phase: M_USER_INFO,
            stop: self.stop,
            found: self.found,
            sealer: self.sealer,
            batch: 1,
            concurrency: 1,
            flush: USER_FLUSH,
            flush_bytes: 0,
            failures_in_a_row: 0,
        };
        let lookup = UserLookup {
            db: self.db,
            latchkey: self.latchkey,
        };
        owed::drain(&l, owed, &lookup).await
    }

    /// A page `history_params` asked for, stored in one transaction with
    /// the stretch it settles. `walk` is left where the page after
    /// starts.
    async fn store_page(&self, channel_id: &str, walk: &mut Walk, resp: Value) -> Result<Page> {
        let params = history_params(channel_id, walk);
        let stretch = &walk.stretch;
        let open_top = stretch.span.hi == END_OF_TIME;
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
        let rows: Vec<MessageInput> = messages
            .iter()
            .filter_map(|m| history_message_input(self.team_id, channel_id, m))
            .collect();

        let has_more = resp.get("has_more").and_then(|v| v.as_bool());
        let cursor = next_cursor(&resp);
        let last = cursor.is_none() || has_more == Some(false);
        let short = cursor.is_none() && has_more == Some(true);
        let in_stretch: Vec<String> = rows
            .iter()
            .map(|m| ts_key(&m.ts))
            .filter(|key| *key >= stretch.span.lo)
            .collect();
        // An open-topped walk that found nothing names no message to
        // cover up to, so it covers the one point where it began: that
        // tells "read, nothing there" from "never read".
        let found_nothing = open_top && walk.top.is_none() && in_stretch.is_empty();
        let covered = settled(&stretch.span, walk.top.as_deref(), &in_stretch, last)
            .or_else(|| {
                (last && found_nothing).then(|| Span::new(&stretch.span.lo, &stretch.span.lo))
            })
            .filter(|_| !short);
        let enumerated = covered
            .as_ref()
            .filter(|_| stretch.reread)
            .map(|span| Enumerated {
                oldest: key_ts(&span.lo),
                latest: key_ts(&span.hi),
                latest_included: walk.top_included,
            });
        let pruned = self
            .db
            .store_history_page(channel_id, &rows, covered.as_ref(), enumerated.as_ref())
            .await?;

        if short {
            return Err(cut_short(M_HISTORY));
        }
        walk.cursor = cursor;
        if let Some(oldest) = in_stretch.into_iter().min() {
            walk.top = Some(oldest);
            walk.top_included = false;
        }
        Ok(Page { rows, pruned, last })
    }

    /// Every page of `conversations.replies` for the thread `key` names:
    /// the thread whole, root copy included, or an error.
    async fn fetch_thread(&self, key: &str) -> Result<Thread> {
        let (_, channel_id, thread_ts) =
            schema_raw::split_key(key).ok_or_else(|| anyhow::anyhow!("{key}: not a thread key"))?;
        let mut base = BTreeMap::new();
        base.insert("channel".to_string(), channel_id.to_string());
        base.insert("ts".to_string(), thread_ts.to_string());
        base.insert("limit".to_string(), MESSAGES_PER_PAGE.to_string());

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
            rows.extend(
                msgs.iter()
                    .filter_map(|m| reply_message_input(self.team_id, channel_id, thread_ts, m)),
            );

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

    /// The bookkeeping [`owed::write_answer`] writes an answer for
    /// `table` with. It reads none of the sizes: the walk asks for one
    /// thread or [`FILE_BATCH`] files at a time, [`AT_ONCE`] at once.
    fn owed_loop(&self, table: &'static str, phase: &'static str) -> owed::Loop<'_> {
        owed::Loop {
            pool: self.db.pool(),
            table,
            phase,
            stop: self.stop,
            found: self.found,
            sealer: self.sealer,
            batch: 1,
            concurrency: 1,
            flush: 1,
            flush_bytes: 0,
            failures_in_a_row: 0,
        }
    }

    fn file_fetcher(&self) -> FileFetcher<'_> {
        FileFetcher {
            db: self.db,
            latchkey: self.latchkey,
            blob_size_limit_bytes: self.blob_size_limit_bytes,
            blake3_by_file: self.blake3_by_file,
            bar: self.bar,
            downloaded: AtomicUsize::new(0),
        }
    }
}

/// The `conversations.history` params for `walk`'s next page.
fn history_params(channel_id: &str, walk: &Walk) -> BTreeMap<String, String> {
    let stretch = &walk.stretch;
    let mut params = BTreeMap::new();
    params.insert("channel".to_string(), channel_id.to_string());
    params.insert("oldest".to_string(), key_ts(&stretch.span.lo));
    params.insert("inclusive".to_string(), stretch.inclusive.to_string());
    params.insert("include_all_metadata".to_string(), "true".to_string());
    params.insert("limit".to_string(), MESSAGES_PER_PAGE.to_string());
    if stretch.span.hi != END_OF_TIME {
        params.insert("latest".to_string(), key_ts(&stretch.span.hi));
    }
    if let Some(c) = &walk.cursor {
        params.insert("cursor".to_string(), c.clone());
    }
    params
}

/// A request the walk has out, with its answer once it comes.
enum Answered {
    Page(usize, Result<Value>),
    Thread(
        Listed,
        std::result::Result<Vec<Fetched<Thread>>, BatchError>,
    ),
    Files(
        Vec<Listed>,
        std::result::Result<Vec<Fetched<File>>, BatchError>,
    ),
}

/// One request out, answering with when it was asked for. Nothing out
/// touches the store: the store has one connection, the writer's, and a
/// request parked holding it would stall the write in front of it.
type Out<'a> = Pin<Box<dyn Future<Output = (Answered, Instant)> + Send + 'a>>;

/// A thread's place in the queue: its latest reply, or its root when the
/// listing names none.
fn thread_reach(listed: &Listed) -> String {
    let root = schema_raw::split_key(&listed.key).map_or("", |(_, _, ts)| ts);
    ts_key(listed.version.as_deref().unwrap_or(root))
}

/// A file edge's place in the queue: its message.
fn file_reach(edge: &Listed) -> String {
    let message = edge.key.rsplit_once('#').map_or("", |(message, _)| message);
    ts_key(schema_raw::split_key(message).map_or("", |(_, _, ts)| ts))
}

/// The threads `rows` list, each at the latest reply its root names:
/// what [`RawDb::threads_listed`] reads back once they are stored.
fn threads_on(rows: &[MessageInput]) -> Vec<Listed> {
    rows.iter()
        .filter(|m| m.is_thread_root)
        .filter(|m| {
            m.payload
                .get("reply_count")
                .and_then(Value::as_i64)
                .is_some_and(|n| n > 0)
        })
        .map(|m| {
            let latest_reply = m.payload.get("latest_reply").and_then(Value::as_str);
            Listed::new(
                schema_raw::slack_thread_key(&m.team_id, &m.channel_id, &m.ts),
                latest_reply,
            )
        })
        .collect()
}

/// Queue what storing `rows` left owed: their threads, and with `media`
/// their files. Each newly owed one is announced on `bar` here, the
/// first the run knows of it.
async fn queue_owed(
    db: &RawDb,
    frontier: &mut Frontier,
    bar: &RunBar,
    rows: &[MessageInput],
    media: bool,
) -> Result<()> {
    let mut new = 0;
    for thread in owed::owed(db.pool(), THREADS, threads_on(rows)).await? {
        new += u64::from(frontier.threads.push(thread_reach(&thread), thread));
    }
    if media {
        new += queue_files(frontier, db.files_owed_by(rows).await?);
    }
    if new > 0 {
        bar.expect(new);
    }
    Ok(())
}

/// Queue file edges, each at its message's `ts`. How many were newly owed.
fn queue_files(frontier: &mut Frontier, files: Vec<(String, Listed)>) -> u64 {
    files
        .into_iter()
        .map(|(ts, file)| u64::from(frontier.files.push(ts_key(&ts), file)))
        .sum()
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
    /// Minutes before a conversation's refresh window is re-read again;
    /// `0` re-reads it every run.
    pub refresh_every_minutes: i64,
    pub members_only: bool,
    /// Also mirror archived conversations. A channel named in `channels`
    /// is mirrored archived or not.
    pub archived: bool,
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
    /// Search for new replies on threads history will not read again
    /// (`reply_search`). Off in [`FetchOptions::new`], whose playback
    /// tests run on the wall clock: the query names the run's days.
    pub search_replies: bool,
    /// `scope = "involves_me"`: walk only the channels where something
    /// involves the account. `None` walks every channel chosen.
    pub involves_me: Option<InvolvesMeOptions>,
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
            refresh_every_minutes: DEFAULT_REFRESH_EVERY_MINUTES,
            members_only: true,
            archived: false,
            media: true,
            dms: false,
            dm_conversations: None,
            blob_size_limit_bytes: None,
            search_replies: false,
            involves_me: None,
            now: Utc::now(),
            progress: datalib_etl::progress::Progress::noop(),
            control: datalib_etl::control::DownloadControl::default(),
        }
    }
}

/// What `scope = "involves_me"` searches for, and how much of a channel
/// around each message it finds a run walks.
#[derive(Debug, Clone)]
pub struct InvolvesMeOptions {
    pub kinds: Vec<datalib_etl_slack_config::InvolvesMe>,
    /// At most zero.
    pub before: ChronoDuration,
    /// `None` walks up to now.
    pub after: Option<ChronoDuration>,
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
        "refresh_every_minutes": opts.refresh_every_minutes,
        "members_only": opts.members_only,
        "archived": opts.archived,
        "media": opts.media,
        "dms": opts.dms,
        "dm_conversations": opts.dm_conversations,
        "blob_size_limit_bytes": opts.blob_size_limit_bytes,
        "search_replies": opts.search_replies,
        "involves_me": opts.involves_me.as_ref().map(|o| json!({
            "kinds": o.kinds.iter().map(|k| k.as_str()).collect::<Vec<_>>(),
            "before_s": o.before.num_seconds(),
            "after_s": o.after.map(|a| a.num_seconds()),
        })),
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
        // The two listings need nothing from each other; a DM's title,
        // which needs the user directory, is worked out once both are in.
        // A listing that fails leaves the stored one.
        let include_archived = opts.archived || opts.channels.is_some();
        let (users, listed) = tokio::join!(
            fetch_users(&db, &now, &setup, &opts.latchkey),
            fetch_channels(
                &db,
                opts.members_only,
                include_archived,
                opts.dms,
                &setup,
                &opts.latchkey,
            ),
        );
        if let Err(e) = users {
            found.push(listing_problem(M_USERS, &e));
        }
        let listed = match listed {
            Ok(listed) => listed,
            // The channels an earlier listing stored are still worth
            // walking; with none stored there is nothing to do at all.
            Err(e) => {
                let stored = db
                    .channels_for_fetch(opts.members_only, include_archived, opts.dms)
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
        let mut targets = plan.targets;

        // `scope = "involves_me"`: the channels with something that
        // involves the account, and the stretches of each to walk. DMs
        // are walked as they would be anyway.
        let mut counts = None;
        let mut wanted: Option<BTreeMap<String, Vec<Span>>> = None;
        if let Some(narrow) = opts.involves_me.as_ref() {
            if opts.control.stop.requested() {
                return Ok(());
            }
            let Some(user_id) = self_user_id.as_deref() else {
                anyhow::bail!(
                    "{M_AUTH_TEST} named no user, so nothing can be searched for as involving it"
                );
            };
            let queries = Query::for_kinds(&narrow.kinds);
            let top = ts_key(&datetime_to_slack_ts(&(now - SEARCH_LAG)));
            let mut searched = InvolvingTotals::default();
            let search = InvolvingSearch {
                db: &db,
                team_id: &team_id,
                user_id,
                latchkey: &opts.latchkey,
                progress: &setup,
            };
            // The search and `client.counts` need nothing from each
            // other, so they go out together.
            let no_params = empty_params();
            let (searched_ok, answer) = tokio::join!(
                search.run(&queries, &since, &top, &mut searched),
                call(M_COUNTS, &no_params, &opts.latchkey),
            );
            // No fallback: walking every channel instead is the cost this
            // scope exists to avoid, so a search that fails fails the run.
            match searched_ok {
                Ok(()) => {}
                Err(e) if interrupted(&e) => return Ok(()),
                Err(e) => {
                    return Err(e.context(
                        "scope = \"involves_me\": the search for the messages that involve \
                         the account failed, so no channel was chosen",
                    ))
                }
            }
            let mentioned = match &answer {
                Ok(answer) => involving::mentioned(answer, &datetime_to_slack_ts(&now)),
                Err(_) => BTreeMap::new(),
            };
            counts = Some(answer);
            let first_dm = targets.len() - plan.dm_targets;
            let candidates: HashSet<String> =
                targets[..first_dm].iter().map(|(c, _)| c.clone()).collect();
            let hits = involving::hits(&db, &queries).await?;
            let chosen = involving::plan(
                &since,
                &hits,
                &mentioned,
                &candidates,
                &involving::Context {
                    before: narrow.before,
                    after: narrow.after,
                },
            );
            info!(
                event = "slack_involving_planned",
                candidates = candidates.len(),
                channels = chosen.wanted.len(),
                hits = hits.len(),
                mentioned = mentioned.len(),
                roots_outside = chosen.roots.len(),
                dm_targets = plan.dm_targets,
                "chose the channels where something involves the account"
            );
            let mut index = 0usize;
            targets.retain(|(cid, _)| {
                let keep = index >= first_dm || chosen.wanted.contains_key(cid);
                index += 1;
                keep
            });
            read_roots_outside(&db, &team_id, &chosen.roots, &opts, &found, &setup).await?;
            wanted = Some(chosen.wanted);
        }

        // The account's own state and the reply search read and write
        // nothing of each other's, so they go out together.
        let account_state = async {
            if opts.control.stop.requested() {
                return Ok(None);
            }
            fetch_account_state(
                &db,
                &targets,
                counts,
                &now,
                &opts.control.stop,
                &setup,
                &opts.latchkey,
            )
            .await
            .map(Some)
        };
        let reply_search = async {
            if !opts.search_replies || opts.control.stop.requested() {
                return;
            }
            let top = ts_key(&datetime_to_slack_ts(&(now - SEARCH_LAG)));
            let search = ReplySearch {
                db: &db,
                team_id: &team_id,
                latchkey: &opts.latchkey,
                progress: &setup,
            };
            let mut totals = ReplySearchTotals::default();
            // A search that fails settles nothing past where it stopped,
            // so what it missed is searched again next run.
            if let Err(e) = search.run(&targets, &since, &top, &mut totals).await {
                if !interrupted(&e) {
                    found.push(listing_problem(M_SEARCH, &e));
                }
            }
            if let Some(sealer) = opts.sealer.as_ref() {
                sealer.wrote(totals.roots_reread as u64).await;
            }
        };
        let (account_state, ()) = tokio::join!(account_state, reply_search);
        let mut open_latest = None;
        if let Some(state) = account_state? {
            found.extend(state.problems);
            open_latest = state.open_latest;
            let account = state.totals;
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

        // What every channel owes, from the store: the stretches of
        // history not covered (and the refresh window's, when due), the
        // threads not held at their latest reply, the files without
        // bytes. A run that was cut off left exactly that, so nothing is
        // carried over from it. A conversation `client.counts` shows
        // quiet walks no history but still gets the threads and files
        // it owes.
        let refresh_every = chrono::Duration::minutes(opts.refresh_every_minutes);
        let first_dm = targets.len() - plan.dm_targets;
        let mut frontier = Frontier::default();
        let mut owed_before: u64 = 0;
        let mut refreshing = vec![false; targets.len()];
        let mut quiet = vec![false; targets.len()];
        // Closed DMs whose check is due, and whether they were ever
        // checked before.
        let mut checking = vec![false; targets.len()];
        let mut rechecking = vec![false; targets.len()];
        let mut closed = 0usize;
        let archived: HashSet<&str> = listed
            .iter()
            .filter(|t| t.is_archived)
            .map(|t| t.id.as_str())
            .collect();
        // Archived channels whose first walk since Slack archived them is
        // due.
        let mut reading_archived = vec![false; targets.len()];
        for (target, (cid, _)) in targets.iter().enumerate() {
            let archived_read = db
                .manifest_sweep_age(&archived_read_key(cid), &now)
                .await?
                .is_some();
            if archived_read && !archived.contains(cid.as_str()) {
                // Listed open again: if Slack archives it later, what was
                // posted in between is read then.
                db.forget_manifest_sweep(&archived_read_key(cid)).await?;
            }
            let newest = match (&open_latest, target >= first_dm) {
                _ if archived.contains(cid.as_str()) => {
                    reading_archived[target] = !archived_read;
                    Newest::Archived {
                        read: archived_read,
                    }
                }
                (None, _) => Newest::Unknown,
                (Some(open), is_dm) => match open.get(cid) {
                    Some(latest) => Newest::At(latest),
                    None if is_dm => {
                        let age = db
                            .manifest_sweep_age(&closed_dm_check_key(cid), &now)
                            .await?;
                        closed += 1;
                        checking[target] = refresh_due(age, CLOSED_DM_CHECK_EVERY);
                        rechecking[target] = checking[target] && age.is_some();
                        Newest::Closed {
                            check_due: checking[target],
                        }
                    }
                    None => Newest::Unknown,
                },
            };
            let due = refresh_due(
                db.manifest_sweep_age(&refresh_sweep_key(cid), &now).await?,
                refresh_every,
            );
            let held = coverage::held(db.pool(), &db::history_scope(cid)).await?;
            let wants = wanted
                .as_ref()
                .and_then(|w| w.get(cid).cloned())
                .unwrap_or_else(|| from_since(&since));
            let walks = to_walk(
                &wants,
                refresh_from.as_deref().filter(|_| due),
                &held,
                newest,
            );
            refreshing[target] = walks.iter().any(|w| w.reread);
            quiet[target] = matches!(newest, Newest::At(_) | Newest::Archived { read: true })
                && walks.iter().all(|w| w.reread);
            frontier
                .walks
                .extend(walks.into_iter().map(|stretch| Walk::new(target, stretch)));
            for thread in owed::owed(db.pool(), THREADS, db.threads_listed(cid).await?).await? {
                owed_before += u64::from(frontier.threads.push(thread_reach(&thread), thread));
            }
            if opts.media {
                for file in db.files_listed(&team_id, cid).await? {
                    owed_before += u64::from(frontier.files.push(file_reach(&file), file));
                }
            }
        }

        let walking: BTreeSet<usize> = frontier.walks.iter().map(|w| w.target).collect();
        info!(
            event = "slack_walks_planned",
            conversations = targets.len(),
            walking = walking.len(),
            quiet = quiet.iter().filter(|q| **q).count(),
            refreshing = refreshing.iter().filter(|r| **r).count(),
            closed_dms = closed,
            closed_dms_checked = checking.iter().filter(|c| **c).count(),
            archived = targets
                .iter()
                .filter(|(cid, _)| archived.contains(cid.as_str()))
                .count(),
            archived_first_read = reading_archived.iter().filter(|r| **r).count(),
            "left out the history with nothing new since what is held"
        );

        // One tick per conversation walked, thread read and file
        // fetched, each announced as it is queued. A message is not one:
        // Slack names no count up front, so it would be done the moment
        // it was known and never be seen waiting.
        let bar = RunBar::new(&opts.progress, walking.len() as u64 + owed_before);
        let mirror = Mirror {
            db: &db,
            team_id: &team_id,
            blob_size_limit_bytes: opts.blob_size_limit_bytes,
            bar: &bar,
            latchkey: &opts.latchkey,
            stop: &opts.control.stop,
            found: &found,
            sealer: opts.sealer.as_ref(),
            blake3_by_file: &blake3_by_file,
        };
        let mut totals = Totals::default();
        let mut messages_in = vec![0usize; targets.len()];
        // Messages a walk found above everything held.
        let mut news_in = vec![0usize; targets.len()];
        let mut failed = vec![false; targets.len()];
        // A conversation `client.counts` shows quiet has answered for its
        // history as surely as one walked.
        let mut walked = (0..targets.len())
            .filter(|t| quiet[*t] && !walking.contains(t))
            .count();
        let at_once = AT_ONCE.min(datalib_etl_web::retry::current_or_default().requests_at_once());
        let threads = Threads {
            mirror: &mirror,
            replies: AtomicUsize::new(0),
            pruned: AtomicUsize::new(0),
            with_files: Mutex::new(Vec::new()),
        };
        let files = mirror.file_fetcher();
        let thread_loop = mirror.owed_loop(THREADS, M_REPLIES);
        let file_loop = mirror.owed_loop(SlackAttachmentRow::TABLE, "files");
        // Answers are written in the order they were asked for, whatever
        // order they come in, so what the frontier sees between requests,
        // and so every request after, does not depend on which answered
        // first.
        let mut out: FuturesOrdered<Out<'_>> = FuturesOrdered::new();
        let stopped = || opts.control.stop.requested();
        loop {
            // Asked to stop: end here, without waiting on what is out or
            // writing what answered since. It stays owed.
            if stopped() {
                break;
            }
            while out.len() < at_once {
                let started = Instant::now();
                let request: Out<'_> = match frontier.next(FILE_BATCH) {
                    Next::Nothing => break,
                    Next::Page(i) => {
                        let walk = &frontier.walks[i];
                        let (cid, name) = &targets[walk.target];
                        bar.doing(&format!("{name}: listing"));
                        let params = history_params(cid, walk);
                        let latchkey = &opts.latchkey;
                        Box::pin(async move {
                            let answer = call(M_HISTORY, &params, latchkey).await;
                            (Answered::Page(i, answer), started)
                        })
                    }
                    Next::Thread(listed) => {
                        let threads = &threads;
                        Box::pin(async move {
                            let answer = threads.fetch(vec![listed.clone()]).await;
                            (Answered::Thread(listed, answer), started)
                        })
                    }
                    Next::Files(owed) => {
                        let objects = files.objects(&owed).await?;
                        let files = &files;
                        Box::pin(async move {
                            let answer = files.fetch_objects(owed.clone(), objects).await;
                            (Answered::Files(owed, answer), started)
                        })
                    }
                };
                out.push_back(request);
            }
            let Some((answered, started)) = out.next().await else {
                break;
            };
            if stopped() {
                break;
            }
            let before = totals.written();
            match answered {
                Answered::Thread(listed, answer) => {
                    let replies_before = threads.replies.load(Ordering::Relaxed);
                    owed::write_answer(&thread_loop, &threads, vec![listed], answer).await?;
                    let replies = threads.replies.load(Ordering::Relaxed) - replies_before;
                    totals.replies += replies;
                    totals.pruned += threads.pruned.swap(0, Ordering::Relaxed);
                    let with_files =
                        std::mem::take(&mut *threads.with_files.lock().expect("not poisoned"));
                    let mut files_found = 0;
                    if opts.media {
                        files_found =
                            queue_files(&mut frontier, db.files_owed_by(&with_files).await?);
                        if files_found > 0 {
                            bar.expect(files_found);
                        }
                    }
                    info!(
                        event = "slack_thread_read",
                        replies,
                        files_found,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "read a thread"
                    );
                }
                Answered::Files(owed, answer) => {
                    let asked = owed.len();
                    let downloaded_before = files.downloaded.load(Ordering::Relaxed);
                    let drained = owed::write_answer(&file_loop, &files, owed, answer).await?;
                    let downloaded = files.downloaded.load(Ordering::Relaxed) - downloaded_before;
                    let outcomes: BTreeMap<&str, usize> = [
                        ("downloaded", downloaded),
                        ("skipped", drained.got.saturating_sub(downloaded)),
                        ("too_large", drained.skipped),
                        ("error", drained.failed),
                    ]
                    .into_iter()
                    .filter(|(_, n)| *n > 0)
                    .collect();
                    for (outcome, n) in &outcomes {
                        *totals.media.entry(outcome.to_string()).or_insert(0) += n;
                    }
                    info!(
                        event = "slack_files_fetched",
                        files = asked,
                        outcomes = ?outcomes,
                        elapsed_ms = started.elapsed().as_millis() as u64,
                        "fetched a batch of files"
                    );
                }
                Answered::Page(i, answer) => {
                    let target = frontier.walks[i].target;
                    let open_top = frontier.walks[i].stretch.span.hi == END_OF_TIME;
                    let (cid, name) = &targets[target];
                    let span = info_span!("channel", channel_name = %name, channel_id = %cid);
                    let stored = match answer {
                        Ok(resp) => {
                            mirror
                                .store_page(cid, &mut frontier.walks[i], resp)
                                .instrument(span)
                                .await
                        }
                        Err(e) => Err(e),
                    };
                    match stored {
                        Ok(page) => {
                            frontier.answered(i, page.last);
                            totals.messages += page.rows.len();
                            totals.pruned += page.pruned;
                            messages_in[target] += page.rows.len();
                            if open_top {
                                news_in[target] += page.rows.len();
                            }
                            queue_owed(&db, &mut frontier, &bar, &page.rows, opts.media).await?;
                        }
                        Err(e) if interrupted(&e) => break,
                        // A channel that failed costs only the rest of its
                        // history: what it did not cover is still owed, by
                        // the store's own account, and its threads and
                        // files are queued with everyone's.
                        Err(e) => {
                            found.push(listing_problem(&format!("{M_HISTORY} {name}"), &e));
                            frontier.end(target);
                            failed[target] = true;
                        }
                    }
                    if !frontier.walking(target) {
                        info!(
                            event = "slack_channel_done",
                            channel = %name,
                            messages = messages_in[target],
                            "walked one channel's history",
                        );
                        bar.did(1);
                        walked += usize::from(!failed[target]);
                        if refreshing[target] && !failed[target] {
                            db.record_manifest_sweep(&refresh_sweep_key(cid), &now)
                                .await?;
                        }
                        if reading_archived[target] && !failed[target] {
                            db.record_manifest_sweep(&archived_read_key(cid), &now)
                                .await?;
                        }
                        if checking[target] && !failed[target] {
                            db.record_manifest_sweep(&closed_dm_check_key(cid), &now)
                                .await?;
                            // The daily check is the fallback behind
                            // `client.counts`; it finding news means a DM
                            // was reopened and closed between runs.
                            if rechecking[target] && news_in[target] > 0 {
                                warn!(
                                    event = "slack_closed_dm_had_news",
                                    channel = %name,
                                    messages = news_in[target],
                                    "a DM closed in the sidebar had messages client.counts did not show"
                                );
                            }
                        }
                    }
                }
            }
            bar.doing(&totals.line());
            if let Some(sealer) = opts.sealer.as_ref() {
                sealer.wrote((totals.written() - before) as u64).await;
            }
        }
        drop(out);
        if stopped() {
            info!(
                event = "slack_interrupted",
                "told to stop; leaving the rest for the next run"
            );
        } else {
            let started = std::time::Instant::now();
            let users = mirror.fetch_users_owed().await?;
            if users != owed::Drained::default() {
                info!(
                    event = "slack_users_looked_up",
                    got = users.got,
                    failed = users.failed,
                    left = users.left,
                    elapsed_ms = started.elapsed().as_millis() as u64,
                    "looked up the authors no user listing had named"
                );
            }
        }
        grand.messages = totals.messages;
        grand.replies = totals.replies;
        grand.pruned = totals.pruned;
        grand.media = totals.media;
        bar.finish();
        // Every channel answering with nothing is a success to Slack, so
        // nothing else would say the mirror is empty.
        if walked > 0 && !opts.control.stop.requested() {
            let held = db.count_messages().await?;
            found.silent((held == 0).then(|| SilentEntry {
                name: "channels".to_string(),
                detail: format!(
                    "{walked} channel(s) read and none holds a message since {}. \
                     If this workspace has messages, check `since` and `channels`.",
                    opts.since
                ),
            }));
        }
        Ok::<(), anyhow::Error>(())
    };

    // On the heap: the walk's state is large enough, in a build without
    // optimisation, to overflow a worker thread's stack beside the
    // futures a caller wraps it in.
    let result = Box::pin(work).await;
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

/// The roots of threads a reply that involves the account is in, where
/// no stretch walked reaches them: each read on its own, so its thread
/// is owed like any other. A root already stored is left alone; one
/// that could not be read is a `problems` row, and still owed next run.
async fn read_roots_outside(
    db: &RawDb,
    team_id: &str,
    roots: &[(String, String)],
    opts: &FetchOptions,
    found: &RunProblems,
    progress: &datalib_etl::progress::Progress,
) -> Result<()> {
    let keys: Vec<String> = roots
        .iter()
        .map(|(cid, root)| schema_raw::slack_thread_key(team_id, cid, root))
        .collect();
    let stored = db.root_latest_replies(&keys).await?;
    let mut read = 0usize;
    for ((cid, root), key) in roots.iter().zip(&keys) {
        if stored.contains_key(key) {
            continue;
        }
        if opts.control.stop.requested() {
            break;
        }
        progress.set_message(&format!("{M_HISTORY} a thread root in {cid}"));
        match reply_search::read_root(team_id, cid, root, &opts.latchkey).await {
            Ok(Some(message)) => {
                db.upsert_messages(&[message]).await?;
                read += 1;
            }
            Ok(None) => {}
            Err(e) if interrupted(&e) => break,
            Err(e) => found.push(listing_problem(&format!("{M_HISTORY} root {key}"), &e)),
        }
    }
    info!(
        event = "slack_involving_roots_read",
        outside = roots.len(),
        read,
        "read the thread roots no stretch walked reaches"
    );
    if let Some(sealer) = opts.sealer.as_ref() {
        sealer.wrote(read as u64).await;
    }
    Ok(())
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
        stretches(
            &from_since(&ts_key(SINCE)),
            refresh_from.map(ts_key).as_deref(),
            held,
        )
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

    /// An open-topped walk that found nothing names no message to settle
    /// up to; `Mirror::page` covers the point it began at instead.
    #[test]
    fn an_empty_walk_of_the_open_top_settles_nothing() {
        assert_eq!(settled(&span(SINCE, END_OF_TIME), None, &[], true), None);
        // A closed stretch that came back empty was read whole.
        let below = span(SINCE, "1735689600.000100");
        let top = ts_key("1735689600.000100");
        assert_eq!(settled(&below, Some(&top), &[], true), Some(below.clone()));
    }

    /// A stretch with a top, `context_after`'s window: the top is asked
    /// for, since nothing read it, and a refresh re-reads only inside
    /// the window however far up what is held reaches.
    #[test]
    fn a_window_is_walked_inside_its_ends() {
        let want = [span("1735689600.000100", "1735776000.000100")];
        assert_eq!(
            stretches(&want, None, &[]),
            [gap("1735689600.000100", "1735776000.000100", true)]
        );
        let bottom = [span("1735689600.000100", "1735700000.000100")];
        assert_eq!(
            stretches(&want, None, &bottom),
            [gap("1735700000.000100", "1735776000.000100", true)]
        );
        let beyond = [span("1735689600.000100", "1735800000.000100")];
        assert_eq!(
            stretches(&want, Some(&ts_key("1735700000.000000")), &beyond),
            [Stretch {
                span: span("1735700000.000000", "1735776000.000100"),
                inclusive: true,
                reread: true,
            }]
        );
    }

    // ── Which conversations are walked ───────────────────────────────

    fn planned_walk(refresh_from: Option<&str>, held: &[Span], newest: Newest) -> Vec<Stretch> {
        to_walk(
            &from_since(&ts_key(SINCE)),
            refresh_from.map(ts_key).as_deref(),
            held,
            newest,
        )
    }

    fn walks(held: &[Span], latest: Option<&str>) -> bool {
        let newest = latest.map_or(Newest::Closed { check_due: false }, Newest::At);
        !planned_walk(None, held, newest).is_empty()
    }

    /// #1374: a DM closed in the sidebar, or open with nothing since
    /// `since`, costs no history call.
    #[test]
    fn a_dm_closed_or_quiet_since_since_is_idle() {
        assert!(!walks(&[], None));
        assert!(!walks(&[], Some("1700000000.000000")));
        // An open DM that never held a message.
        assert!(!walks(&[], Some("0000000000.000000")));
        assert!(walks(&[], Some(SINCE)));
        assert!(walks(&[], Some("1735689600.000100")));
    }

    #[test]
    fn a_dm_walked_up_to_its_newest_message_is_idle_until_a_newer_one() {
        let held = [span(SINCE, "1735689600.000100")];
        assert!(!walks(&held, Some("1735689600.000100")));
        assert!(walks(&held, Some("1735689600.000200")));
        assert!(!walks(&held, None));
    }

    /// A closed DM due its check is walked whole: never walked, or not
    /// for a day. Before, a closed DM was never walked, and a first sync
    /// lost what it held.
    #[test]
    fn a_closed_dm_due_its_check_is_walked_whole() {
        let held = [span(SINCE, "1735689600.000100")];
        let due = Newest::Closed { check_due: true };
        assert_eq!(
            planned_walk(None, &[], due),
            [gap(SINCE, END_OF_TIME, true)]
        );
        assert_eq!(
            planned_walk(None, &held, due),
            [gap("1735689600.000100", END_OF_TIME, false)]
        );
    }

    /// A closed DM not due its check still owes a gap under what is held.
    #[test]
    fn a_closed_dm_between_checks_walks_only_a_gap_under_what_is_held() {
        let held = [span("1735689600.000050", "1735689600.000100")];
        assert_eq!(
            planned_walk(None, &held, Newest::Closed { check_due: false }),
            [gap(SINCE, "1735689600.000050", true)]
        );
    }

    /// A walk cut off, or a `since` moved earlier, leaves a gap under
    /// what is held, which the newest message alone would not show.
    #[test]
    fn a_dm_with_a_gap_under_its_newest_message_is_walked() {
        let cut_off = [span("1735689600.000050", "1735689600.000100")];
        assert!(walks(&cut_off, Some("1735689600.000100")));
    }

    /// #1423: every channel cost a history call each run, so a routine
    /// sync of 126 quiet channels took 80 s. A channel `client.counts`
    /// shows with nothing newer than what is held walks nothing; one it
    /// does not list is walked as before.
    #[test]
    fn a_channel_counts_shows_quiet_is_not_walked_and_an_unlisted_one_is() {
        let held = [span(SINCE, "1735689600.000100")];
        assert!(planned_walk(None, &held, Newest::At("1735689600.000100")).is_empty());
        assert_eq!(
            planned_walk(None, &held, Newest::Unknown),
            [gap("1735689600.000100", END_OF_TIME, false)]
        );
    }

    /// An archived channel is read whole once; after that it asks for
    /// nothing newer and re-reads no window, and only a gap under what
    /// is held (a `since` moved earlier, a walk cut off) is still owed.
    #[test]
    fn an_archived_channel_read_once_owes_only_a_gap_under_what_is_held() {
        let window = Some("1735000000.000000");
        let held = [span(SINCE, "1735689600.000300")];
        assert_eq!(
            planned_walk(window, &held, Newest::Archived { read: false }).len(),
            2,
            "its first read since it was archived also re-reads its window"
        );
        assert!(planned_walk(window, &held, Newest::Archived { read: true }).is_empty());
        let cut_off = [span("1735689600.000050", "1735689600.000300")];
        assert_eq!(
            planned_walk(window, &cut_off, Newest::Archived { read: true }),
            [gap(SINCE, "1735689600.000050", true)]
        );
    }

    /// A quiet conversation still re-reads its window when one is due,
    /// and only that: the open top above its newest message is not asked.
    #[test]
    fn a_quiet_conversation_due_a_refresh_rereads_only_its_window() {
        let held = [span(SINCE, "1735689600.000300")];
        let window = Some("1735000000.000000");
        let quiet = planned_walk(window, &held, Newest::At("1735689600.000300"));
        assert_eq!(quiet.len(), 1);
        assert!(quiet[0].reread);
        // A newer message brings the open top back alongside it.
        assert_eq!(
            planned_walk(window, &held, Newest::At("1735689600.000400")).len(),
            2
        );
        // A closed DM is not refreshed: it is not walked at all.
        assert!(planned_walk(window, &held, Newest::Closed { check_due: false }).is_empty());
    }

    #[test]
    fn a_refresh_is_due_once_the_last_one_is_old_enough() {
        let hour = chrono::Duration::hours(1);
        assert!(refresh_due(None, hour));
        assert!(!refresh_due(Some(chrono::Duration::minutes(59)), hour));
        assert!(refresh_due(Some(hour), hour));
        // `0` re-reads every run, and a clock set back reads as recent.
        assert!(refresh_due(
            Some(chrono::Duration::zero()),
            chrono::Duration::zero()
        ));
        assert!(!refresh_due(Some(chrono::Duration::minutes(-5)), hour));
        assert!(refresh_due(
            Some(chrono::Duration::minutes(-5)),
            chrono::Duration::zero()
        ));
    }

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
            is_archived: false,
            dm_user_ids: Vec::new(),
        }
    }

    fn im(id: &str, user_id: &str) -> FetchTarget {
        FetchTarget {
            id: id.into(),
            name: None,
            is_dm: true,
            is_archived: false,
            dm_user_ids: vec![user_id.into()],
        }
    }

    fn mpim(id: &str, name: &str, members: &[&str]) -> FetchTarget {
        FetchTarget {
            id: id.into(),
            name: Some(name.into()),
            is_dm: true,
            is_archived: false,
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
            is_archived: false,
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
