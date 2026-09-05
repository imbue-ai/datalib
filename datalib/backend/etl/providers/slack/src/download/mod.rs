//! Slack downloader entry point.
//!
//! Captures Slack data into a single doltlite db at
//! `<data_root>/<name>/raw/entities.doltlite_db` — one row per workspace
//! (`auth.test`), user, channel, message, reply page, and attachment
//! edge, plus the shared `cas_objects` blob store. See `db.rs` and
//! `schema_raw.rs` for the table layout.
//!
//! Resume cursor: derived at startup from the DB.
//! `RawDb::ts_bounds_by_channel` gives the per-channel `max(ts)` we've
//! ever recorded, and the next forward pass starts there. The trailing
//! refresh window re-queries the last N days; idempotent upserts
//! collapse no-op refresh passes to zero writes.
//!
//! Because that cursor answers "where do I start?" on its own, a config
//! change that *widens* what should be on disk would otherwise be
//! silently ignored — the classic case being `since` moved to an
//! earlier date, which the forward walk can't express because it only
//! ever moves forward. [`Adjustments`] closes that gap: the
//! scope-affecting params are recorded via
//! [`datalib_etl::scope_config`] after each successful run, and the
//! next run diffs them to schedule a bounded backfill (or, for a
//! relaxed blob knob, a re-walk). Narrowing is always a no-op — the
//! store is a superset and nothing in the pipeline deletes.

pub mod api;
pub mod db;
pub mod schema_raw;
pub mod shapes;

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use chrono::{DateTime, Duration as ChronoDuration, Utc};
use datalib_etl::blob_cas::CasEdgeAccumulator;
use serde_json::{json, Value};
use tracing::{info, info_span, instrument, warn, Instrument};

use api::{call_slack, SlackCall, SlackError};
use datalib_etl::events;
use datalib_etl::http::LatchkeySettings;
use datalib_etl::scope_config;
pub use db::{
    block_on_load_all, db_path_for, FetchTarget, LoadedMessage, LoadedRaw, MessageInput, RawDb,
    TsBounds, UserDirectoryEntry,
};
use shapes::{M_AUTH_TEST, M_CHANNELS, M_HISTORY, M_REPLIES, M_USERS};

pub const DEFAULT_SINCE: &str = "2024-01-01";
pub const DEFAULT_REFRESH_WINDOW_DAYS: i64 = 30;

/// Max age of a successful channel/user list sweep before we refetch.
/// Slack `conversations.list` is Tier-2 rate-limited (~20 req/min), so
/// a workspace with thousands of channels costs tens of seconds per
/// refetch even on warm-cache runs.
pub const MANIFEST_TTL: chrono::Duration = chrono::Duration::hours(6);

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

async fn call(
    method: &str,
    params: &BTreeMap<String, String>,
    latchkey: &LatchkeySettings,
) -> Result<Value> {
    let SlackCall { response, .. } = call_slack(method, params, latchkey)
        .await
        .map_err(|e: SlackError| anyhow::anyhow!("{}", e))?;
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
    );
    Ok((team_id, self_user_id))
}

/// `types` for `conversations.list`. `im` / `mpim` are appended only
/// when DMs are wanted: the parameter is what decides whether Slack
/// hands us DM conversations at all, so leaving it at the channel pair
/// is the enforcement point for `dms = false`, not just a filter.
fn conversation_types(dms: bool) -> &'static str {
    if dms {
        "public_channel,private_channel,im,mpim"
    } else {
        "public_channel,private_channel"
    }
}

#[instrument(skip(db, progress))]
async fn fetch_channels(
    db: &RawDb,
    members_only: bool,
    include_archived: bool,
    dms: bool,
    progress: &datalib_etl::progress::Progress,
    latchkey: &LatchkeySettings,
) -> Result<Vec<FetchTarget>> {
    // `dms` is part of the key, not just the request: turning DMs on
    // asks for a strictly wider `types`, and a sweep recorded under the
    // narrower one would suppress the refetch for up to MANIFEST_TTL —
    // the run would then find no DM rows and quietly mirror nothing.
    let sweep_key = format!("channels:archived={include_archived}:dms={dms}");
    if let Some(age) = db.manifest_sweep_age(&sweep_key).await? {
        if age < MANIFEST_TTL {
            let age_s = age.num_seconds().max(0);
            info!(
                event = "slack_fetch_channels_skipped",
                reason = "ttl",
                age_s = age_s,
                ttl_s = MANIFEST_TTL.num_seconds(),
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
    );
    db.record_manifest_sweep(&sweep_key).await?;
    db.channels_for_fetch(members_only, include_archived, dms)
        .await
}

#[instrument(skip_all)]
async fn fetch_users(
    db: &RawDb,
    progress: &datalib_etl::progress::Progress,
    latchkey: &LatchkeySettings,
) -> Result<usize> {
    let sweep_key = "users";
    if let Some(age) = db.manifest_sweep_age(sweep_key).await? {
        if age < MANIFEST_TTL {
            let age_s = age.num_seconds().max(0);
            info!(
                event = "slack_fetch_users_skipped",
                reason = "ttl",
                age_s = age_s,
                ttl_s = MANIFEST_TTL.num_seconds(),
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
    );
    db.record_manifest_sweep(sweep_key).await?;
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
// Which conversations this run walks.
// ---------------------------------------------------------------------------

/// A `dm_users` entry as written, normalized for matching: leading `@`
/// dropped, trimmed, lowercased.
fn normalize_dm_entry(spec: &str) -> String {
    spec.trim().trim_start_matches('@').trim().to_lowercase()
}

/// The `dm_users` allowlist resolved against the mirrored user
/// directory.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct DmAllowlist {
    /// Slack user ids whose DMs are in scope.
    user_ids: std::collections::BTreeSet<String>,
    /// Entries that matched nobody. Warned about rather than ignored:
    /// a typo'd handle otherwise mirrors nothing and looks identical to
    /// "that person never DM'd you".
    unmatched: Vec<String>,
}

/// Resolve `dm_users` entries to Slack user ids.
///
/// An entry matches a user id, handle, display name or real name,
/// case-insensitively, with an optional leading `@`. One entry may
/// legitimately resolve to several users (two people can share a
/// display name); all of them are kept, since the alternative is
/// silently dropping one person's DMs.
fn resolve_dm_users(entries: &[String], users: &[UserDirectoryEntry]) -> DmAllowlist {
    let mut out = DmAllowlist::default();
    for spec in entries {
        let want = normalize_dm_entry(spec);
        if want.is_empty() {
            continue;
        }
        let mut hit = false;
        for u in users {
            let candidates = [
                Some(u.id.as_str()),
                u.name.as_deref(),
                u.display_name.as_deref(),
                u.real_name.as_deref(),
            ];
            if candidates
                .into_iter()
                .flatten()
                .any(|c| c.trim().to_lowercase() == want)
            {
                out.user_ids.insert(u.id.clone());
                hit = true;
            }
        }
        if !hit {
            out.unmatched.push(spec.clone());
        }
    }
    out
}

/// What [`select_targets`] decided.
#[derive(Debug, Default, PartialEq, Eq)]
struct TargetPlan {
    /// `(channel_id, label)` for every conversation to walk. Channels
    /// first, then DMs.
    targets: Vec<(String, String)>,
    /// How many of `targets` are DMs — the tail of the vec.
    dm_targets: usize,
}

/// Split the listed conversations into the ones this run walks.
///
/// The two scoping knobs are independent, and that is the whole point:
/// `channels` filters channels by name, `dm_users` filters DMs by
/// person. Running the channel-name filter over DMs — which is what
/// happens if you treat one list as covering both — drops every DM,
/// because a DM has no name to match.
///
/// A group DM is in scope when *any* of its members is on the
/// allowlist: allowlisting Riker means "the conversations I have with
/// Riker", and the three-way with Riker and Data is one of them.
fn select_targets(
    listed: &[FetchTarget],
    channels: Option<&[String]>,
    dm_allow: Option<&DmAllowlist>,
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
                if let Some(t) = by_name.get(name) {
                    plan.targets.push((t.id.clone(), name.to_string()));
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

    for t in listed.iter().filter(|t| t.is_dm) {
        let counterparts = schema_raw::dm_counterparts(&t.dm_user_ids, self_user_id);
        if let Some(allow) = dm_allow {
            if !counterparts.iter().any(|u| allow.user_ids.contains(u)) {
                continue;
            }
        }
        let label =
            schema_raw::dm_display_name(&counterparts, t.name.as_deref(), &t.id, user_labels);
        plan.targets.push((t.id.clone(), label));
        plan.dm_targets += 1;
    }

    plan
}

// ---------------------------------------------------------------------------
// Config-change adjustments.
// ---------------------------------------------------------------------------

/// Scope key for this provider's [`datalib_etl::scope_config`] blob.
/// Slack's incremental state is per-channel (`MAX(ts)` in `messages`),
/// but every knob we remember applies workspace-wide, so one row per
/// source is the right grain.
const SCOPE_CONFIG_KEY: &str = "slack:download";

/// Blob keys. Named so the writer below and the readers in
/// [`Adjustments::plan`] can't drift — a typo in either half degrades
/// silently to "no information, plan no work", which is the exact
/// failure this machinery exists to eliminate.
const K_SINCE: &str = "since";
const K_MEDIA: &str = "media";
const K_BLOB_CAP: &str = "blob_size_limit_bytes";

/// The subset of [`FetchOptions`] that decides *which data lands on
/// disk*, recorded after a successful run so the next one can spot a
/// widening the per-channel watermark would otherwise swallow.
///
/// Deliberately excludes:
/// - `channels` / `members_only` — a newly listed channel has no rows,
///   so `channel_latest_ts` is `None` and it cold-starts from `since`
///   without any help from us.
/// - `dms` / `dm_users` — same reason, one level up. Turning DMs on
///   lists conversations that have no message rows at all, so each one
///   cold-starts from `since` on its own. (What *does* need help is the
///   `conversations.list` sweep TTL, which would otherwise serve the
///   pre-DM listing for up to six hours — handled by keying the sweep
///   marker on `dms`, in `fetch_channels`.)
/// - `refresh_window_days` — already re-applied on every run.
/// - `conv`-style one-offs and paths — not scope-affecting.
fn scope_config_blob(opts: &FetchOptions) -> Value {
    json!({
        K_SINCE: opts.since,
        K_MEDIA: opts.media,
        K_BLOB_CAP: opts.blob_size_limit_bytes,
    })
}

/// What this run has to do differently because the config widened since
/// the run that produced the store's current contents.
///
/// Both fields default to "nothing to do", which is what an absent or
/// unreadable blob must produce — see `scope_config`'s module docs on
/// why a first upgrade can't be allowed to stampede every mirror into a
/// full re-download.
#[derive(Debug, Default, Clone)]
struct Adjustments {
    /// `since` moved earlier: walk `[since_ts, oldest_stored_ts]` for
    /// each channel that already has history. The forward watermark is
    /// untouched — this only fills in below the floor.
    backfill_below_oldest: bool,
    /// A blob knob was relaxed (`media` off→on, or a raised/lifted size
    /// cap): re-walk each channel from `since_ts` instead of resuming
    /// at its watermark. Attachment rows only exist for messages walked
    /// while the knob was on, so there is nothing to backfill in place —
    /// the messages have to come past `download_files_for_messages`
    /// again.
    force_full_walk: bool,
}

impl Adjustments {
    /// Whether any adjustment is in play.
    ///
    /// `#[cfg(test)]` because only the tests below consult it — the
    /// production path branches on the individual flags. Without the
    /// gate, clippy's `dead_code` fails the non-test build.
    #[cfg(test)]
    fn any(&self) -> bool {
        self.backfill_below_oldest || self.force_full_walk
    }

    /// Whether pass B can skip a thread because its replies are already
    /// mirrored.
    ///
    /// Normally "stored `latest_reply` is at or past what the API
    /// advertises" is sufficient. Under [`Self::force_full_walk`] it is
    /// not: reply attachments are downloaded *only* inside
    /// `paginate_replies`, so a thread that is fully mirrored
    /// message-wise still has unfetched files hanging off it when the
    /// blob knob that skipped them is later relaxed. Re-walking pass A
    /// alone would fetch top-level attachments and silently miss every
    /// in-thread one.
    fn thread_up_to_date(&self, api_latest: Option<&str>, stored: Option<&str>) -> bool {
        if self.force_full_walk {
            return false;
        }
        matches!((api_latest, stored), (Some(api), Some(s)) if s >= api)
    }

    /// Whether a completed run has actually satisfied the config it
    /// planned for, and may therefore record it.
    ///
    /// Per-channel failures are swallowed into a `warn!` so one bad
    /// channel can't sink a whole sync, which means `Ok(())` from the
    /// work future does *not* imply every channel was covered. Recording
    /// the blob anyway would make the next run see no widening and drop
    /// the scheduled backfill permanently — unlike the per-channel
    /// watermark, which self-heals because it is derived from stored
    /// rows rather than from bookkeeping.
    fn run_satisfied_config(run_ok: bool, channel_failures: usize) -> bool {
        run_ok && channel_failures == 0
    }

    /// Diff the recorded blob against this run's options.
    ///
    /// Only *widenings* produce work. A narrowed knob leaves an on-disk
    /// superset, and nothing in the pipeline deletes, so it is always a
    /// no-op. A `since` that fails to parse is treated as no
    /// information rather than an error: the caller has already
    /// validated the current value, and a garbage *stored* value must
    /// not fail an otherwise-good sync.
    fn plan(prev: Option<&Value>, opts: &FetchOptions) -> Self {
        let mut out = Self::default();
        let Some(prev) = prev else {
            return out;
        };

        if let Some(stored) = prev.get(K_SINCE).and_then(Value::as_str) {
            match (
                parse_iso_or_utc_date(stored),
                parse_iso_or_utc_date(&opts.since),
            ) {
                (Ok(before), Ok(now)) if now < before => {
                    out.backfill_below_oldest = true;
                    info!(
                        event = "slack_since_widened",
                        from = stored,
                        to = %opts.since,
                        "backfilling below each channel's oldest stored message",
                    );
                }
                (Err(e), _) => warn!(
                    event = "slack_stored_since_unparseable",
                    stored = stored,
                    error = %e,
                    "ignoring stored since",
                ),
                _ => {}
            }
        }

        if scope_config::turned_on(Some(prev), K_MEDIA, opts.media) {
            out.force_full_walk = true;
            info!(
                event = "slack_media_turned_on",
                "re-walking history so existing messages' attachments get fetched",
            );
        }

        // Gated on `media`: the cap is only consulted inside
        // `download_files_for_messages`, which the whole walk skips when
        // blobs are off. Re-walking every channel (and re-paginating
        // every mirrored thread) against a rate-limited API to download
        // exactly zero bytes is pure cost.
        if opts.media
            && scope_config::limit_relaxed(Some(prev), K_BLOB_CAP, opts.blob_size_limit_bytes)
        {
            out.force_full_walk = true;
            info!(
                event = "slack_blob_limit_relaxed",
                limit = ?opts.blob_size_limit_bytes,
                "re-walking history so previously-oversize attachments get fetched",
            );
        }

        out
    }
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
    channel_oldest_ts: Option<&str>,
    adjust: &Adjustments,
    latest_reply_by_thread: &std::collections::HashMap<(String, String), String>,
    now: &DateTime<Utc>,
    download_blobs: bool,
    blob_size_limit_bytes: Option<u64>,
    totals: &mut ChannelTotals,
    blake3_by_file: &mut std::collections::HashMap<String, String>,
    progress: &datalib_etl::progress::Progress,
    latchkey: &LatchkeySettings,
) -> Result<()> {
    // Per-channel attachment accumulator: every (message, file)
    // reference is appended, the BlobBundle carries one byte set per
    // file_id, and the end-of-channel flush writes both the CAS
    // (via put_many) and `slack_attachments` (via bulk_upsert_in_tx).
    let mut attach = CasEdgeAccumulator::new();

    // Pass A: list every history page, upsert top-level messages, and
    // download per-page media (preserves the existing commit-as-we-go
    // semantics for Ctrl-C safety). Thread replies are deferred so
    // we can announce a known total to the inner bar before starting
    // the long-tail fetch.
    let mut collected: Vec<Value> = Vec::new();

    // Resume at the channel's watermark when it has one — unless a
    // relaxed blob knob means the already-stored messages have to come
    // back past `download_files_for_messages`, in which case we walk
    // the whole configured range again. Upserts are idempotent, so the
    // cost is API calls, not duplicate rows.
    let (forward_oldest, inclusive) = match channel_latest_ts.filter(|_| !adjust.force_full_walk) {
        Some(ts) => (ts.to_string(), false),
        None => (since_ts.to_string(), true),
    };
    // The resume decision, per conversation, in the run's own log.
    //
    // "Why did this conversation re-walk history it already has?" is
    // otherwise unanswerable after the fact: the only observable is a
    // message count in the summary, and every explanation for a
    // non-zero one — no stored watermark, a widened `since`, a relaxed
    // blob knob — produces the identical number. These four fields
    // separate them. `resumed = false` with a `watermark` present means
    // an adjustment forced the re-walk; `resumed = false` with none
    // means the conversation had nothing stored and cold-started.
    info!(
        event = "slack_channel_walk_planned",
        channel = %channel_id,
        watermark = channel_latest_ts.unwrap_or("-"),
        oldest = %forward_oldest,
        inclusive = inclusive,
        resumed = channel_latest_ts.is_some() && !adjust.force_full_walk,
        force_full_walk = adjust.force_full_walk,
        backfill_below_oldest = adjust.backfill_below_oldest,
    );
    list_history(
        db,
        team_id,
        channel_id,
        &forward_oldest,
        inclusive,
        None,
        download_blobs,
        blob_size_limit_bytes,
        totals,
        &mut attach,
        blake3_by_file,
        progress,
        &mut collected,
        latchkey,
    )
    .await?;

    // Skipped under `force_full_walk`: the pass above already re-walked
    // `[since_ts, now]`, which strictly contains the trailing window, so
    // running it again would just double the API calls on exactly the
    // run that is already the expensive one.
    if refresh_window_days > 0 && !adjust.force_full_walk {
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
                    blob_size_limit_bytes,
                    totals,
                    &mut attach,
                    blake3_by_file,
                    progress,
                    &mut collected,
                    latchkey,
                )
                .await?;
            }
        }
    }

    // Backfill pass: `since` moved earlier than the run that built this
    // channel's history, so the window below our oldest stored message
    // was never fetched. Walk `[since_ts, oldest]` to fill it in. Runs
    // before pass B so backfilled thread roots land in `collected` and
    // get their replies fetched like any other message.
    //
    // Skipped when `force_full_walk` already re-walked the whole range
    // above, and when the channel has no history (the cold-start arm
    // started at `since_ts` already).
    if adjust.backfill_below_oldest && !adjust.force_full_walk {
        if let Some(oldest) = channel_oldest_ts {
            if since_ts < oldest {
                info!(
                    event = "slack_backfill_window",
                    channel = %channel_id,
                    from = since_ts,
                    to = oldest,
                );
                list_history(
                    db,
                    team_id,
                    channel_id,
                    since_ts,
                    true,
                    Some(oldest),
                    download_blobs,
                    blob_size_limit_bytes,
                    totals,
                    &mut attach,
                    blake3_by_file,
                    progress,
                    &mut collected,
                    latchkey,
                )
                .await?;
            }
        }
    }

    // Pass B: thread replies for threads whose latest_reply advanced.
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
            if adjust.thread_up_to_date(api_latest, stored.map(String::as_str)) {
                return None;
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
        if adjust.thread_up_to_date(api_latest, stored.map(String::as_str)) {
            continue;
        }
        let before = totals.replies;
        paginate_replies(
            db,
            team_id,
            channel_id,
            ts,
            download_blobs,
            blob_size_limit_bytes,
            totals,
            &mut attach,
            blake3_by_file,
            latchkey,
        )
        .await?;
        let fetched = totals.replies.saturating_sub(before) as u64;
        progress.inc(fetched);
        let media_downloaded = totals.media.get("downloaded").copied().unwrap_or(0);
        progress.set_message(&format!(
            "msgs={} replies={} media={}",
            totals.messages, totals.replies, media_downloaded
        ));
    }

    // End-of-channel flush: CAS put_many + slack_attachments bulk
    // upsert. Mirrors chatgpt/claude's per-conv flush pattern.
    if let Err(e) = api::flush_channel_attachments(db, &attach).await {
        warn!(event = "slack_attachment_flush_err", channel = %channel_id, error = %e);
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
/// thread fetches begin.
#[allow(clippy::too_many_arguments)]
async fn list_history(
    db: &RawDb,
    team_id: &str,
    channel_id: &str,
    oldest_ts: &str,
    inclusive: bool,
    latest_ts: Option<&str>,
    download_blobs: bool,
    blob_size_limit_bytes: Option<u64>,
    totals: &mut ChannelTotals,
    attach: &mut CasEdgeAccumulator,
    blake3_by_file: &mut std::collections::HashMap<String, String>,
    progress: &datalib_etl::progress::Progress,
    collected: &mut Vec<Value>,
    latchkey: &LatchkeySettings,
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
        let resp = call(M_HISTORY, &params, latchkey).await?;
        let messages: Vec<Value> = resp
            .get("messages")
            .and_then(|v| v.as_array())
            .map(|a| a.to_vec())
            .unwrap_or_default();
        // One line per request actually issued, with the bounds that
        // were sent. Pairs with `slack_channel_walk_planned`: that says
        // what the walk intended, this says what went on the wire and
        // what came back, so a re-fetch can be attributed to a specific
        // call rather than inferred from a total.
        info!(
            event = "slack_history_page",
            channel = %channel_id,
            oldest = params.get("oldest").map(String::as_str).unwrap_or("-"),
            latest = params.get("latest").map(String::as_str).unwrap_or("-"),
            inclusive = params.get("inclusive").map(String::as_str).unwrap_or("-"),
            cursor = params.get("cursor").map(String::as_str).unwrap_or("-"),
            returned = messages.len(),
        );

        let rows: Vec<MessageInput> = messages
            .iter()
            .filter_map(|m| history_message_input(team_id, channel_id, m))
            .collect();
        db.upsert_messages(&rows).await?;
        totals.messages += messages.len();
        progress.inc(messages.len() as u64);

        if download_blobs {
            let counts = api::download_files_for_messages(
                db,
                team_id,
                channel_id,
                &messages,
                None,
                attach,
                blake3_by_file,
                blob_size_limit_bytes,
                latchkey,
            )
            .await?;
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
/// and records a `replies_pages` row so the next sync can skip.
#[allow(clippy::too_many_arguments)]
async fn paginate_replies(
    db: &RawDb,
    team_id: &str,
    channel_id: &str,
    thread_ts: &str,
    download_blobs: bool,
    blob_size_limit_bytes: Option<u64>,
    totals: &mut ChannelTotals,
    attach: &mut CasEdgeAccumulator,
    blake3_by_file: &mut std::collections::HashMap<String, String>,
    latchkey: &LatchkeySettings,
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
        let resp = call(M_REPLIES, &p, latchkey).await?;
        let msgs: Vec<Value> = resp
            .get("messages")
            .and_then(|v| v.as_array())
            .map(|a| a.to_vec())
            .unwrap_or_default();

        let rows: Vec<MessageInput> = msgs
            .iter()
            .filter_map(|m| reply_message_input(team_id, channel_id, thread_ts, m))
            .collect();
        db.upsert_messages(&rows).await?;
        for m in &msgs {
            if let Some(ts) = m.get("ts").and_then(|v| v.as_str()) {
                if ts != thread_ts && last_seen_reply.as_deref().is_none_or(|prev| ts > prev) {
                    last_seen_reply = Some(ts.to_string());
                }
            }
        }
        totals.replies += msgs.len().saturating_sub(1);

        if download_blobs {
            let counts = api::download_files_for_messages(
                db,
                team_id,
                channel_id,
                &msgs,
                Some(thread_ts),
                attach,
                blake3_by_file,
                blob_size_limit_bytes,
                latchkey,
            )
            .await?;
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

// ---------------------------------------------------------------------------
// Public entry point.
// ---------------------------------------------------------------------------

pub struct FetchOptions {
    /// Which latchkey identity the download authenticates as, from the
    /// source's `latchkey_settings:` block.
    pub latchkey: LatchkeySettings,
    pub db_path: PathBuf,
    pub db: Option<RawDb>,
    pub channels: Option<Vec<String>>,
    pub since: String,
    pub refresh_window_days: i64,
    pub members_only: bool,
    pub media: bool,
    /// Mirror direct messages (1:1 and group). Off by default — see
    /// `SlackApiSync::dms`.
    pub dms: bool,
    /// Restrict DMs to conversations with these people. Only consulted
    /// when `dms` is on; `SlackApiSync::validate` rejects the other
    /// combination before it gets here.
    pub dm_users: Option<Vec<String>>,
    pub blob_size_limit_bytes: Option<u64>,
    pub progress: datalib_etl::progress::Progress,
    pub control: datalib_etl::control::DownloadControl,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            db_path: PathBuf::new(),
            db: None,
            latchkey: LatchkeySettings::default(),
            channels: None,
            since: DEFAULT_SINCE.to_string(),
            refresh_window_days: DEFAULT_REFRESH_WINDOW_DAYS,
            members_only: true,
            media: true,
            dms: false,
            dm_users: None,
            blob_size_limit_bytes: None,
            progress: datalib_etl::progress::Progress::noop(),
            control: datalib_etl::control::DownloadControl::default(),
        }
    }
}

#[derive(serde::Serialize)]
pub struct FetchSummary {
    pub messages: usize,
    pub replies: usize,
    pub media: BTreeMap<String, usize>,
}

#[instrument(skip_all, fields(db = %opts.db_path.display()))]
pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db_path = db_path_for(&opts.db_path);
    let _ = datalib_etl::latchkey::ensure_curl_dispatch();
    // `owned` is "we opened this pool, so we close it" — see the close
    // below.
    let (db, owned) = match opts.db.clone() {
        Some(db) => (db, false),
        None => (
            RawDb::open(&db_path)
                .await
                .with_context(|| format!("open raw db {}", db_path.display()))?,
            true,
        ),
    };

    if opts.control.reset_and_redownload {
        tracing::info!(event = "slack_reset_and_redownload");
        db.reset().await.context("reset raw db before redownload")?;
    }
    if opts.control.refetch_blobs {
        tracing::info!(event = "slack_refetch_blobs");
        db.clear_blob_hashes()
            .await
            .context("clear slack_attachments.blake3 before refetch")?;
    }

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
        "dms": opts.dms,
        "dm_users": opts.dm_users,
        "blob_size_limit_bytes": opts.blob_size_limit_bytes,
    });
    let run = datalib_etl::download_run::DownloadRun::start(db.pool(), &run_config).await?;

    // Diff this run's scope-affecting params against the ones that
    // produced the store's current contents. `None` (fresh store, or one
    // written before `sync_scope_config` existed) plans no adjustments.
    let scope_cfg = scope_config_blob(&opts);
    let prior_scope_cfg = scope_config::load_or_none(db.pool(), SCOPE_CONFIG_KEY).await;
    let adjust = Adjustments::plan(prior_scope_cfg.as_ref(), &opts);

    let t_scan = std::time::Instant::now();
    let channel_ts_bounds = db.ts_bounds_by_channel().await?;
    let latest_reply_map = db.latest_reply_by_thread().await?;
    // Run-scoped `(file_id → blake3)` cache: loaded once up-front so
    // the per-file dedupe check inside `download_one_file` is a
    // HashMap hit instead of a SQLite round trip queued behind preceding
    // multi-MB CAS commits on the single-connection doltlite pool.
    // Successful downloads insert into it so later files in the same
    // run hit the cache without re-fetching.
    let mut blake3_by_file = db.load_attachment_blake3s().await?;
    info!(
        event = "slack_resume_scan_done",
        channels_with_history = channel_ts_bounds.len(),
        threads_with_replies = latest_reply_map.len(),
        attachments_with_bytes = blake3_by_file.len(),
        elapsed_ms = t_scan.elapsed().as_millis() as u64,
    );

    let mut grand = FetchSummary {
        messages: 0,
        replies: 0,
        media: BTreeMap::new(),
    };
    // Channels whose export errored. A per-channel failure is warned and
    // stepped over so one bad channel can't sink the sync, which means
    // the work future can return `Ok` on a run that did NOT cover
    // everything the config asked for — see `run_satisfied_config`.
    let mut channel_failures: usize = 0;

    let work = async {
        let setup = opts.progress.child("setup");
        setup.set_message("starting");
        let t_setup = std::time::Instant::now();
        let (team_id, self_user_id) = fetch_self(&db, &setup, &opts.latchkey).await?;
        // Users before channels: a DM is identified by its counterpart,
        // so both the `dm_users` allowlist and the DM progress labels
        // need the user directory to already be mirrored.
        fetch_users(&db, &setup, &opts.latchkey).await?;
        let listed = fetch_channels(
            &db,
            opts.members_only,
            opts.channels.is_some(),
            opts.dms,
            &setup,
            &opts.latchkey,
        )
        .await?;
        setup.finish(&format!(
            "setup done in {}ms",
            t_setup.elapsed().as_millis() as u64
        ));

        // Only loaded when DMs are in play — for a channels-only run it
        // is a whole table scan nothing would read.
        let (user_labels, dm_allow) = if opts.dms {
            let directory = db.user_directory().await?;
            let labels: BTreeMap<String, String> = directory
                .iter()
                .map(|u| (u.id.clone(), u.label()))
                .collect();
            let allow = opts.dm_users.as_ref().map(|entries| {
                let resolved = resolve_dm_users(entries, &directory);
                if !resolved.unmatched.is_empty() {
                    // Silence here would be indistinguishable from
                    // "you have no DMs with that person".
                    warn!(
                        event = "slack_dm_users_unmatched",
                        entries = ?resolved.unmatched,
                        directory_size = directory.len(),
                        "no mirrored user matches these `dm_users` entries — \
                         their DMs will not be mirrored",
                    );
                }
                resolved
            });
            (labels, allow)
        } else {
            (BTreeMap::new(), None)
        };

        let plan = select_targets(
            &listed,
            opts.channels.as_deref(),
            dm_allow.as_ref(),
            &user_labels,
            self_user_id.as_deref(),
        );
        info!(
            event = "slack_export_planned",
            channels = plan.targets.len() - plan.dm_targets,
            dms = opts.dms,
            dm_targets = plan.dm_targets,
            media = opts.media,
        );
        let targets = plan.targets;

        opts.progress.set_length(Some(targets.len() as u64));
        for (cid, name) in &targets {
            opts.progress.set_message(name);
            let span = info_span!("channel", channel_name = %name, channel_id = %cid);
            let mut totals = ChannelTotals::default();
            let inner = opts.progress.child(&format!("slack: {name}"));
            inner.set_message("listing");
            let result = export_channel(
                &db,
                &team_id,
                cid,
                &since_ts,
                opts.refresh_window_days,
                channel_ts_bounds.get(cid).map(|b| b.latest.as_str()),
                channel_ts_bounds.get(cid).map(|b| b.oldest.as_str()),
                &adjust,
                &latest_reply_map,
                &now,
                opts.media,
                opts.blob_size_limit_bytes,
                &mut totals,
                &mut blake3_by_file,
                &inner,
                &opts.latchkey,
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
                Err(e) => {
                    channel_failures += 1;
                    warn!(event = "slack_channel_failed", channel = %name, error = %e);
                }
            }
        }
        Ok::<(), anyhow::Error>(())
    };

    let result = work.await;
    // Record the config only once this run has actually satisfied it, so
    // a failed, partial, or cancelled sync leaves the previous blob in
    // place and the next run retries the adjustment. Bookkeeping
    // failures are logged and swallowed: they must never mask the run's
    // own error.
    scope_config::store_if_satisfied(
        db.pool(),
        SCOPE_CONFIG_KEY,
        &scope_cfg,
        Adjustments::run_satisfied_config(result.is_ok(), channel_failures),
    )
    .await;
    run.finish(&result, &grand).await;
    // Close the pool if — and only if — we opened it. A caller that
    // handed us a `db` owns its lifetime (the processor shares one pool
    // with its `RawStoreSession`, which closes it in `finish`); a
    // caller that did not gets a pool nothing would ever close.
    //
    // That mattered: doltlite's HEAD, working set and active branch are
    // per-connection, so two live connections to one file are two
    // writers, and the second one's `dolt_commit` can fail with
    // `commit conflict: another connection committed to this branch`.
    // sqlx does not close a dropped pool's connections synchronously,
    // so "it goes out of scope here" is not the same as "it is closed"
    // — and the next `open` of the same file may race the one we left
    // behind.
    if owned {
        db.pool().close().await;
    }
    result?;

    info!(
        event = "slack_export_complete",
        messages = grand.messages,
        replies = grand.replies,
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

    fn opts(since: &str, media: bool, blob_size_limit_bytes: Option<u64>) -> FetchOptions {
        FetchOptions {
            since: since.to_string(),
            media,
            blob_size_limit_bytes,
            ..Default::default()
        }
    }

    /// The steady state: same config as last run plans no work. Guards
    /// the round trip `scope_config_blob` → store → load → `plan`, which
    /// is what every unchanged sync exercises.
    #[test]
    fn unchanged_config_plans_nothing() {
        let o = opts("2024-01-01", true, Some(1000));
        let prev = scope_config_blob(&o);
        assert!(!Adjustments::plan(Some(&prev), &o).any());
    }

    #[test]
    fn absent_prior_config_plans_nothing() {
        // Every data root in the field on first upgrade. Must not
        // trigger a backfill.
        let o = opts("2020-01-01", true, None);
        assert!(!Adjustments::plan(None, &o).any());
    }

    // ── since ────────────────────────────────────────────────────────

    #[test]
    fn since_moved_earlier_schedules_backfill_only() {
        let prev = scope_config_blob(&opts("2024-01-01", true, None));
        let plan = Adjustments::plan(Some(&prev), &opts("2023-01-01", true, None));
        assert!(plan.backfill_below_oldest);
        // A widened `since` never needs the expensive re-walk — the
        // forward watermark is still valid.
        assert!(!plan.force_full_walk);
    }

    #[test]
    fn since_moved_later_is_a_noop() {
        let prev = scope_config_blob(&opts("2024-01-01", true, None));
        // Narrowing leaves an on-disk superset; nothing to fetch.
        assert!(!Adjustments::plan(Some(&prev), &opts("2025-01-01", true, None)).any());
    }

    #[test]
    fn since_compares_instants_not_strings() {
        // `2024-01-01` and its RFC 3339 spelling are the same instant, so
        // rewriting the config in the other format must not backfill.
        let prev = scope_config_blob(&opts("2024-01-01", true, None));
        let plan = Adjustments::plan(Some(&prev), &opts("2024-01-01T00:00:00Z", true, None));
        assert!(!plan.any());
    }

    #[test]
    fn unparseable_stored_since_is_ignored() {
        let mut prev = scope_config_blob(&opts("2024-01-01", true, None));
        prev[K_SINCE] = json!("not-a-date");
        // No information beats guessing: a garbage stored value must not
        // fail the sync or provoke a backfill.
        assert!(!Adjustments::plan(Some(&prev), &opts("2020-01-01", true, None)).any());
    }

    // ── media ────────────────────────────────────────────────────────

    #[test]
    fn media_turned_on_forces_full_walk() {
        let prev = scope_config_blob(&opts("2024-01-01", false, None));
        let plan = Adjustments::plan(Some(&prev), &opts("2024-01-01", true, None));
        // Attachment rows only exist for messages walked with media on,
        // so the messages have to come back past the download path.
        assert!(plan.force_full_walk);
    }

    #[test]
    fn media_turned_off_is_a_noop() {
        let prev = scope_config_blob(&opts("2024-01-01", true, None));
        assert!(!Adjustments::plan(Some(&prev), &opts("2024-01-01", false, None)).any());
    }

    // ── blob_size_limit_bytes ────────────────────────────────────────

    #[test]
    fn raised_blob_cap_forces_full_walk() {
        let prev = scope_config_blob(&opts("2024-01-01", true, Some(1000)));
        let plan = Adjustments::plan(Some(&prev), &opts("2024-01-01", true, Some(5000)));
        assert!(plan.force_full_walk);
    }

    #[test]
    fn lifted_blob_cap_forces_full_walk() {
        let prev = scope_config_blob(&opts("2024-01-01", true, Some(1000)));
        let plan = Adjustments::plan(Some(&prev), &opts("2024-01-01", true, None));
        assert!(plan.force_full_walk);
    }

    #[test]
    fn relaxed_blob_cap_with_media_off_is_a_noop() {
        // The cap is only consulted inside `download_files_for_messages`,
        // which the walk skips entirely when blobs are off. Re-walking to
        // download zero bytes is pure rate-limit burn.
        let prev = scope_config_blob(&opts("2024-01-01", false, Some(1000)));
        assert!(!Adjustments::plan(Some(&prev), &opts("2024-01-01", false, Some(5000))).any());
        assert!(!Adjustments::plan(Some(&prev), &opts("2024-01-01", false, None)).any());
    }

    #[test]
    fn lowered_blob_cap_is_a_noop() {
        let prev = scope_config_blob(&opts("2024-01-01", true, Some(5000)));
        assert!(!Adjustments::plan(Some(&prev), &opts("2024-01-01", true, Some(1000))).any());
    }

    // ── thread reply skip ────────────────────────────────────────────

    #[test]
    fn mirrored_thread_is_skipped_normally() {
        let plan = Adjustments::default();
        assert!(plan.thread_up_to_date(Some("100.000000"), Some("100.000000")));
        assert!(plan.thread_up_to_date(Some("100.000000"), Some("101.000000")));
    }

    #[test]
    fn advanced_thread_is_never_skipped() {
        let plan = Adjustments::default();
        assert!(!plan.thread_up_to_date(Some("101.000000"), Some("100.000000")));
        // Never-fetched thread, or an API response without `latest_reply`:
        // fetch rather than silently drop.
        assert!(!plan.thread_up_to_date(Some("101.000000"), None));
        assert!(!plan.thread_up_to_date(None, Some("100.000000")));
    }

    #[test]
    fn force_full_walk_re_walks_mirrored_threads() {
        // Reply attachments are downloaded only inside `paginate_replies`,
        // so a relaxed blob knob has to re-enter fully-mirrored threads or
        // it silently misses every in-thread file.
        let plan = Adjustments {
            force_full_walk: true,
            ..Default::default()
        };
        assert!(!plan.thread_up_to_date(Some("100.000000"), Some("100.000000")));
    }

    #[test]
    fn backfill_alone_leaves_thread_skipping_intact() {
        // A widened `since` adds older messages; it says nothing about
        // threads already mirrored above the floor.
        let plan = Adjustments {
            backfill_below_oldest: true,
            ..Default::default()
        };
        assert!(plan.thread_up_to_date(Some("100.000000"), Some("100.000000")));
    }

    // ── recording the blob ───────────────────────────────────────────

    #[test]
    fn partial_run_does_not_record_config() {
        // Per-channel failures are swallowed, so `Ok` does not imply full
        // coverage. Recording anyway would drop the scheduled backfill
        // permanently for the channels that failed.
        assert!(!Adjustments::run_satisfied_config(true, 1));
        assert!(!Adjustments::run_satisfied_config(false, 0));
        assert!(!Adjustments::run_satisfied_config(false, 3));
    }

    #[test]
    fn clean_run_records_config() {
        assert!(Adjustments::run_satisfied_config(true, 0));
    }

    // ── blob contents ────────────────────────────────────────────────

    #[test]
    fn blob_records_only_scope_affecting_params() {
        let o = FetchOptions {
            since: "2024-01-01".into(),
            channels: Some(vec!["general".into()]),
            refresh_window_days: 30,
            members_only: false,
            dms: true,
            dm_users: Some(vec!["picard".into()]),
            ..Default::default()
        };
        let blob = scope_config_blob(&o);
        let obj = blob.as_object().expect("blob is an object");
        // A newly listed channel cold-starts on its own, and the refresh
        // window is re-applied every run — recording either would only
        // provoke pointless re-walks.
        assert!(!obj.contains_key("channels"));
        assert!(!obj.contains_key("refresh_window_days"));
        assert!(!obj.contains_key("members_only"));
        // Same reason for the DM knobs: a newly listed DM has no rows,
        // so it cold-starts from `since` unaided. What a widened `dms`
        // *does* need is a fresh `conversations.list` sweep, which is
        // handled by the sweep key, not by this blob.
        assert!(!obj.contains_key("dms"));
        assert!(!obj.contains_key("dm_users"));
        assert_eq!(obj.len(), 3, "unexpected keys in blob: {obj:?}");
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

    fn allow(entries: &[&str]) -> DmAllowlist {
        let owned: Vec<String> = entries.iter().map(|s| s.to_string()).collect();
        resolve_dm_users(&owned, &directory())
    }

    #[test]
    fn dm_users_match_every_kind_of_name() {
        // Handle, id, display name, real name — and the `@` a person
        // will type out of habit.
        for spec in ["picard", "@picard", "U1", "Captain", "Jean-Luc Picard"] {
            let a = allow(&[spec]);
            assert_eq!(
                a.user_ids.iter().cloned().collect::<Vec<_>>(),
                vec!["U1".to_string()],
                "{spec:?} should resolve to U1"
            );
            assert!(a.unmatched.is_empty(), "{spec:?}: {:?}", a.unmatched);
        }
    }

    #[test]
    fn dm_user_matching_is_case_insensitive() {
        assert_eq!(allow(&["  @PICARD "]).user_ids.len(), 1);
        assert_eq!(allow(&["jean-luc picard"]).user_ids.len(), 1);
    }

    /// A typo'd handle otherwise mirrors nothing and is indistinguishable
    /// from "you have no DMs with that person".
    #[test]
    fn unmatched_dm_users_are_reported() {
        let a = allow(&["picard", "@q"]);
        assert_eq!(a.user_ids.iter().cloned().collect::<Vec<_>>(), vec!["U1"]);
        assert_eq!(a.unmatched, vec!["@q".to_string()]);
    }

    /// A user with no real_name or display_name is still reachable by
    /// handle — `users.list` leaves both unset for plenty of accounts.
    #[test]
    fn dm_user_with_only_a_handle_resolves() {
        assert_eq!(
            allow(&["data"])
                .user_ids
                .iter()
                .cloned()
                .collect::<Vec<_>>(),
            vec!["U3"]
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

    /// A 1:1 DM: no name, one counterpart in `user`.
    fn im(id: &str, user_id: &str) -> FetchTarget {
        FetchTarget {
            id: id.into(),
            name: None,
            is_dm: true,
            dm_user_ids: vec![user_id.into()],
        }
    }

    /// A group DM: Slack's composite handle plus a `members` array that
    /// includes the account itself (U1 here). Shape confirmed against
    /// the live API.
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

    /// The 1:1 DM with Riker, and — because `members` is on the wire —
    /// the group DM he is in. Allowlisting a person means "the
    /// conversations I have with that person", both shapes included.
    #[test]
    fn allowlist_keeps_conversations_with_the_named_person() {
        let a = allow(&["riker"]);
        let plan = select_targets(&listed(), None, Some(&a), &labels(), SELF);
        assert_eq!(walked(&plan), vec!["C1", "D1", "G1"]);
        assert_eq!(plan.dm_targets, 2);
    }

    /// An allowlist that resolved to nobody must mirror no DMs — not
    /// fall open to all of them.
    #[test]
    fn allowlist_matching_nobody_walks_no_dms() {
        let a = allow(&["@q"]);
        assert!(a.user_ids.is_empty());
        let plan = select_targets(&listed(), None, Some(&a), &labels(), SELF);
        assert_eq!(walked(&plan), vec!["C1"]);
        assert_eq!(plan.dm_targets, 0);
    }

    /// Allowlisting *yourself* must not sweep in every group DM you are
    /// in — `members` includes you, so the match runs against
    /// counterparts, not raw participants.
    #[test]
    fn allowlisting_yourself_does_not_match_every_group_dm() {
        let a = allow(&["picard"]);
        let plan = select_targets(&listed(), None, Some(&a), &labels(), SELF);
        assert_eq!(
            walked(&plan),
            vec!["C1"],
            "self is not a counterpart in any of these DMs"
        );
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
