//! ChatGPT downloader entry point. Port of `src/ingest/chatgpt_web.py`.

pub mod api;
pub mod db;
pub mod schema_raw;

use std::collections::{HashMap, HashSet};
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::DateTime;
use datalib_etl::blob_cas::CasEdgeAccumulator;
use datalib_etl::bulk::bulk_upsert_in_tx;
use datalib_etl::doltlite_raw::WirePayload;
use datalib_etl::download_problems::DownloadProblem;
use datalib_etl::download_run::DownloadRun;
use datalib_etl::http::LatchkeySettings;
use datalib_etl::http::IMPERSONATE_MARKER_HEADER;
use datalib_etl::latchkey::latchkey_curl_command;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_problems::Reason;
use datalib_time::IsoOffsetTimestamp;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::time::sleep;
use tracing::{info, info_span, instrument, warn, Instrument};

pub use api::{ChatGPTClient, ChatGPTError};
use datalib_etl::blob_cas::CasEdgeRow as _;
pub use db::{db_path_for, LoadedConversation, LoadedRaw, RawDb};
use schema_raw::{ConversationAttachmentRow, ConversationRow as ConversationRowSchema, MeRow};

/// Inter-fetch sleep. ChatGPT doesn't appear to throttle us at any
/// polite rate; 100ms keeps us from looking like a tight loop without
/// doubling per-conv latency on top of ~400ms GETs.
pub const SLEEP_BETWEEN: Duration = Duration::from_millis(100);
pub const PAGE_SIZE: usize = 100;

/// File-timeout for attachment GETs through the latchkey shim.
const ATTACH_FILE_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Which latchkey identity the download authenticates as, from the
    /// source's `latchkey_settings:` block. Default = the only stored
    /// account for the service.
    pub latchkey: LatchkeySettings,
    /// The store this run writes into, opened and closed by the caller.
    /// A download never opens a store of its own: one writer per file
    /// (`datalib/backend/etl/README.md` § "One writer per file, by
    /// construction").
    pub db: RawDb,
    /// Seals what has been written so far, so render can start on the
    /// early conversations while the rest are still arriving. `None` --
    /// the default, and what every test uses -- commits once at the end.
    pub sealer: Option<datalib_etl::raw_store::Sealer>,
    pub max_pages: Option<usize>,
    pub limit: Option<usize>,
    pub sleep_between: Duration,
    /// Only sync conversations whose `update_time` is at or after this
    /// instant (RFC 3339 or `YYYY-MM-DD`, assumed UTC). Older
    /// conversations are never detail-fetched, and the listing walk
    /// stops early once a page ends past the cutoff (the listing is
    /// `order=updated`, newest first). `None` → sync everything.
    /// Ignored in `conv_uuids` mode.
    pub since: Option<String>,
    /// When non-empty, fetch only these conversation ids. Skips the
    /// paginated listing walk; `/me` is still fetched (cheap, captures
    /// account id).
    pub conv_uuids: Vec<String>,
    /// The run-pinned `--now`, so deterministic builds get a stable
    /// stamp; `None` samples the clock.
    pub now: Option<String>,
    pub progress: datalib_etl::progress::Progress,
    /// Cross-provider knobs (the checkpoint cadence, the stop flag).
    pub control: datalib_etl::control::DownloadControl,
}

impl FetchOptions {
    /// Every field defaulted except the store, which has none to give:
    /// it is a live handle the caller opens and closes.
    pub fn new(db: RawDb) -> Self {
        Self {
            latchkey: LatchkeySettings::default(),
            db,
            sealer: None,
            max_pages: None,
            limit: None,
            sleep_between: Duration::ZERO,
            since: None,
            conv_uuids: Vec::new(),
            now: None,
            progress: datalib_etl::progress::Progress::noop(),
            control: datalib_etl::control::DownloadControl::default(),
        }
    }
}

#[derive(Debug, Default, Serialize)]
pub struct FetchSummary {
    pub fetched: usize,
    pub skipped: usize,
    /// Listed items ignored because their `update_time` predates the
    /// configured `since`. Items behind an early-stopped listing walk
    /// are never listed at all and are not counted here. Not counted
    /// in `skipped` (which means "in scope and already up to date").
    pub out_of_scope: usize,
    pub errors: usize,
    pub listing: usize,
    /// Conversations a *complete* listing did not name — deleted upstream.
    /// Always 0 when the listing walk stopped early.
    pub pruned: usize,
    pub new_blobs: usize,
    pub skipped_blobs: usize,
    pub failed_blobs: usize,
    pub requests: u64,
    pub network_seconds: f64,
}

#[instrument(skip_all, fields(
    db = %opts.db.pool().connect_options().get_filename().display()
))]
pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let (pool, stop) = (opts.db.pool().clone(), opts.control.stop.clone());
    let sealer = opts.sealer.clone();
    run_problems::collecting_sealed(&pool, &stop, sealer.as_ref(), |found| download(opts, found))
        .await
}

async fn download(opts: FetchOptions, found: RunProblems) -> Result<FetchSummary> {
    let _ = datalib_etl::latchkey::ensure_curl_router();
    let db = opts.db.clone();

    // Canonicalized to whole-second epoch, the same grain the
    // skip-check compares `update_time`s at (see `update_time_secs`).
    let since_secs = opts
        .since
        .as_deref()
        .map(parse_since_secs)
        .transpose()
        .with_context(|| format!("sync.since {:?}", opts.since))?;

    let run_config = json!({
        "max_pages": opts.max_pages,
        "limit": opts.limit,
        "since": opts.since,
        "conv_uuids": opts.conv_uuids,
    });
    let run = DownloadRun::start(db.pool(), &run_config).await?;

    // One `now` per fetch — threaded into every bulk upsert so all
    // `<table>_bookkeeping.fetched_at_utc` stamps from a single sync share
    // a timestamp. The sync orchestrator passes its `--now` here so
    // deterministic builds get a stable stamp.
    let now = match &opts.now {
        Some(s) => datalib_time::parse_strict(s).context("--now")?,
        None => IsoOffsetTimestamp::now_local(),
    };

    let mut client = ChatGPTClient::with_latchkey(opts.latchkey.clone());
    let mut summary = FetchSummary::default();

    // Run-scoped `(file_id → blake3)` cache: loaded once up-front so
    // the per-file dedupe check inside `fetch_attachments` is a
    // HashMap hit instead of a SQLite round trip. Successful
    // downloads insert into it so files referenced by multiple
    // conversations in the same run hit the cache on every reference
    // after the first.
    let mut blake3_by_file = db.load_attachment_blake3s().await?;

    let work = async {
        // /me — cheap, also pins the account id we report under.
        let me = client
            .me()
            .await
            .map_err(|e| anyhow::anyhow!("fetch /me: {e}"))?;
        upsert_me(&db, &me, &now).await?;
        info!(
            event = "chatgpt_me",
            email = me.get("email").and_then(|v| v.as_str()).unwrap_or(""),
            id = me.get("id").and_then(|v| v.as_str()).unwrap_or(""),
            "signed in as this account"
        );

        let mut walk = Walk::new(found);
        if !opts.conv_uuids.is_empty() {
            fetch_named(
                &mut client,
                &db,
                &opts,
                &mut summary,
                &mut blake3_by_file,
                &now,
                &mut walk,
            )
            .await?;
        } else {
            fetch_listed(
                &mut client,
                &db,
                &opts,
                since_secs,
                &mut summary,
                &mut blake3_by_file,
                &now,
                &mut walk,
            )
            .await?;
        }
        if !walk.rate_limited && !opts.control.stop.requested() {
            retry_attachments(
                &mut client,
                &db,
                &opts,
                &mut summary,
                &mut blake3_by_file,
                &mut walk,
            )
            .await?;
        }
        // A rate limit may have left named conversations unchecked, so
        // their `config:` rows stand, and it ended the run before every
        // listing and phase was tried.
        if walk.rate_limited {
            walk.found.cut_short();
        } else {
            walk.found.config(std::mem::take(&mut walk.config_problems));
        }
        Ok::<(), anyhow::Error>(())
    };

    let result = work.await;
    summary.requests = client.requests;
    summary.network_seconds = client.network_seconds;
    run.finish(&result, &summary).await;
    result?;
    Ok(summary)
}

/// What one run could not do, and what the retry pass needs to know
/// about the walk.
struct Walk {
    found: RunProblems,
    config_problems: Vec<DownloadProblem>,
    /// Conversations whose attachments the walk already tried this run.
    attachments_tried: HashSet<String>,
    /// The give-up policy tripped; every further request would too.
    rate_limited: bool,
}

/// Why a conversation's attachments were not all tried.
enum Cut {
    Stopped,
    /// The give-up guard tripped, with why.
    RateLimited(String),
}

impl Walk {
    fn new(found: RunProblems) -> Self {
        Self {
            found,
            config_problems: Vec::new(),
            attachments_tried: HashSet::new(),
            rate_limited: false,
        }
    }

    fn rate_limited(&mut self, fetched: usize, left: usize, reason: &str) {
        self.rate_limited = true;
        self.found.phase(
            "conversations",
            format!("rate-limited after {fetched} fetched; {left} left for the next run: {reason}"),
        );
    }
}

/// `conv_uuids`: exactly the named conversations, no listing.
async fn fetch_named(
    client: &mut ChatGPTClient,
    db: &RawDb,
    opts: &FetchOptions,
    summary: &mut FetchSummary,
    blake3_by_file: &mut HashMap<String, String>,
    now: &IsoOffsetTimestamp,
    walk: &mut Walk,
) -> Result<()> {
    opts.progress.set_length(Some(opts.conv_uuids.len() as u64));
    for (i, raw) in opts.conv_uuids.iter().enumerate() {
        if opts.control.stop.requested() {
            info!(
                event = "chatgpt_interrupted",
                "told to stop; leaving the rest for the next run"
            );
            break;
        }
        opts.progress.inc(1);
        opts.progress.set_message(raw);
        let target = datalib_etl::ids::normalize_id_token(raw);
        match client.get_conversation(&target).await {
            Ok(full) => {
                let cut = save_conversation(
                    client,
                    db,
                    opts,
                    &target,
                    &full,
                    summary,
                    blake3_by_file,
                    now,
                )
                .await?;
                match cut {
                    None => {}
                    Some(Cut::Stopped) => break,
                    Some(Cut::RateLimited(reason)) => {
                        walk.rate_limited(summary.fetched, opts.conv_uuids.len() - i, &reason);
                        break;
                    }
                }
                walk.attachments_tried.insert(target.clone());
                info!(event = "chatgpt_fetch_single_ok", raw = raw, id = %target, "fetched one conversation by id");
            }
            // The transport refused it because of the stop.
            Err(_) if opts.control.stop.requested() => break,
            Err(ChatGPTError::RateLimited { reason, .. }) => {
                walk.rate_limited(summary.fetched, opts.conv_uuids.len() - i, &reason);
                break;
            }
            Err(ChatGPTError::Permanent(msg)) if msg.contains("HTTP 404") => {
                walk.config_problems.push(DownloadProblem::not_found(
                    "conv_uuids",
                    raw,
                    format!("chatgpt.com has no conversation with this id: {msg}"),
                ));
            }
            Err(ChatGPTError::Permanent(msg)) => {
                warn!(event = "chatgpt_fetch_error", raw = raw, id = %target, error = %msg, "a conversation could not be fetched");
                db.record_conversation_error(&target, &msg).await?;
                summary.errors += 1;
            }
        }
    }
    Ok(())
}

/// The listing walk, then every listed conversation that is missing or
/// stale.
#[allow(clippy::too_many_arguments)]
async fn fetch_listed(
    client: &mut ChatGPTClient,
    db: &RawDb,
    opts: &FetchOptions,
    since_secs: Option<i64>,
    summary: &mut FetchSummary,
    blake3_by_file: &mut HashMap<String, String>,
    now: &IsoOffsetTimestamp,
    walk: &mut Walk,
) -> Result<()> {
    opts.progress.set_message("listing conversations");
    let Listing {
        items: listing,
        complete: listing_complete,
        failed,
        rate_limited,
    } = list_all_conversations(client, opts.max_pages, since_secs, &opts.progress)
        .instrument(info_span!("chatgpt_list"))
        .await;
    if let Some(e) = failed {
        if opts.control.stop.requested() {
            return Ok(());
        }
        // With nothing listed and nothing stored there is nothing to
        // fall back on: the run did nothing at all.
        if listing.is_empty() && !db.has_any_conversation().await? {
            return Err(anyhow::anyhow!("list conversations: {e}"));
        }
        walk.found.listing("conversations", e);
        // Every detail fetch would be refused the same way.
        if rate_limited {
            walk.rate_limited = true;
            return Ok(());
        }
    }
    info!(
        event = "chatgpt_listing",
        convs = listing.len(),
        complete = listing_complete,
        "listed the conversations"
    );
    summary.listing = listing.len();

    // A complete walk is an authoritative census of the account, so a
    // conversation we hold that it did not name has been deleted on
    // chatgpt.com. An incomplete one says nothing: the pages it never
    // asked for are full of conversations that still exist, which is
    // why this is gated rather than always-on. With `since` configured
    // most runs stop early and prune nothing, which is the conservative
    // side to err on.
    if listing_complete {
        let keep: HashSet<String> = listing
            .iter()
            .filter_map(|c| c.get("id").and_then(|v| v.as_str()))
            .map(String::from)
            .collect();
        summary.pruned = db.prune_conversations(&keep).await?;
    }

    // Skip-check: bulk-read existing `(id, update_time)` for every
    // listed id, then compare to the listing's update_time. Rows
    // we don't have at all → missing. Rows whose stored update_time
    // differs from the listing's → stale. Both fall into the work
    // queue; everything else is up-to-date and skipped.
    let listed_ids: Vec<&str> = listing
        .iter()
        .filter_map(|c| c.get("id").and_then(|v| v.as_str()))
        .collect();
    let existing = db.existing_update_times(&listed_ids).await?;

    // Prioritize: missing > stale > already-good. Same intent as
    // the JSONL implementation's "spend our 429 budget on new
    // work" ordering.
    let mut missing: Vec<&Value> = Vec::new();
    let mut stale: Vec<&Value> = Vec::new();
    let mut up_to_date: usize = 0;
    for item in &listing {
        let Some(cid) = item.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        // `since` scope filter: out-of-scope items are never
        // detail-fetched. Items with an unparseable `update_time`
        // fall through in scope (fetch rather than silently drop).
        // The filter only gates fetching — already-stored rows are
        // untouched — so moving `since` further back later
        // backfills the newly-in-scope conversations as missing.
        if let (Some(cutoff), Some(api_secs)) = (
            since_secs,
            item.get("update_time").and_then(update_time_secs),
        ) {
            if api_secs < cutoff {
                summary.out_of_scope += 1;
                continue;
            }
        }
        match existing.get(cid) {
            None => missing.push(item),
            // Canonicalize both sides to a whole-second epoch before
            // comparing. The stored value is the *detail* endpoint's
            // Unix-epoch float; `item`'s is the *listing* endpoint's
            // ISO-8601 string — comparing the raw JSON encodings
            // never matches, so every conversation looks stale and
            // gets re-fetched (see `update_time_secs`). Either side
            // failing to canonicalize falls through to `stale`, the
            // safe (re-fetch) direction.
            Some(stored) => {
                let stored_secs = stored_update_time_secs(stored);
                let api_secs = item.get("update_time").and_then(update_time_secs);
                match (stored_secs, api_secs) {
                    (Some(a), Some(b)) if a == b => up_to_date += 1,
                    _ => stale.push(item),
                }
            }
        }
    }
    info!(
        event = "chatgpt_priority_split",
        missing = missing.len(),
        stale = stale.len(),
        up_to_date = up_to_date,
        out_of_scope = summary.out_of_scope,
        "sorted the listing into what to fetch"
    );
    summary.skipped += up_to_date;

    let ordered: Vec<&Value> = missing.into_iter().chain(stale).collect();
    opts.progress.set_length(Some(ordered.len() as u64));
    for (i, item) in ordered.iter().enumerate() {
        // Asked to stop: the conversation that just landed sealed with
        // its blobs, so end here.
        if opts.control.stop.requested() {
            info!(
                event = "chatgpt_interrupted",
                "told to stop; leaving the rest for the next run"
            );
            break;
        }
        opts.progress.inc(1);
        if let Some(limit) = opts.limit {
            if summary.fetched + summary.errors >= limit {
                info!(
                    event = "chatgpt_limit_reached",
                    limit = limit,
                    "reached the configured fetch limit; stopping here"
                );
                break;
            }
        }
        let Some(cid) = item.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        opts.progress.set_message(cid);
        match client.get_conversation(cid).await {
            Ok(full) => {
                let cut =
                    save_conversation(client, db, opts, cid, &full, summary, blake3_by_file, now)
                        .await?;
                match cut {
                    None => {}
                    Some(Cut::Stopped) => break,
                    Some(Cut::RateLimited(reason)) => {
                        walk.rate_limited(summary.fetched, ordered.len() - i, &reason);
                        break;
                    }
                }
                walk.attachments_tried.insert(cid.to_string());
                if opts.sleep_between > Duration::ZERO {
                    sleep(opts.sleep_between).await;
                }
            }
            Err(_) if opts.control.stop.requested() => break,
            // Every later request would be refused too. What is left is
            // still missing or stale, so the next run's skip-check
            // queues it again.
            Err(ChatGPTError::RateLimited { path, reason }) => {
                warn!(
                    event = "chatgpt_rate_limit_giveup",
                    path = %path,
                    reason = %reason,
                    fetched = summary.fetched,
                    "giving up on this request after the rate-limit retries"
                );
                walk.rate_limited(summary.fetched, ordered.len() - i, &reason);
                break;
            }
            Err(ChatGPTError::Permanent(msg)) => {
                warn!(event = "chatgpt_fetch_error", cid = cid, error = %msg, "a conversation could not be fetched");
                db.record_conversation_error(cid, &msg).await?;
                summary.errors += 1;
            }
        }
    }
    Ok(())
}

/// Store one fetched conversation with its attachments, and seal. A
/// [`Cut`] when the attachments were cut short: nothing of it is written,
/// so the next run finds it missing or stale and starts it over.
#[allow(clippy::too_many_arguments)]
async fn save_conversation(
    client: &mut ChatGPTClient,
    db: &RawDb,
    opts: &FetchOptions,
    id: &str,
    full: &Value,
    summary: &mut FetchSummary,
    blake3_by_file: &mut HashMap<String, String>,
    now: &IsoOffsetTimestamp,
) -> Result<Option<Cut>> {
    let full = canonicalize_conversation_payload(full);
    let attach =
        match fetch_attachments(client, &full, summary, blake3_by_file, &opts.control.stop).await {
            Ok(attach) => attach,
            Err(cut) => return Ok(Some(cut)),
        };
    let (title, update_time) = title_and_update_time(&full);
    let payload = serde_json::to_string(&full).context("serialize conversation")?;
    upsert_conversations(
        db,
        &[ConversationUpsert {
            id: id.to_string(),
            title,
            update_time,
            payload,
        }],
        now,
    )
    .await?;
    summary.fetched += 1;
    flush_attachments(db, &attach).await?;
    // The store is consistent here and nowhere earlier: the conversation
    // row and the blobs it names have both landed. Sealing between the
    // two would publish a message pointing at bytes no reader can
    // resolve.
    if let Some(sealer) = opts.sealer.as_ref() {
        sealer.wrote(1).await;
    }
    Ok(None)
}

/// The walk reaches a conversation's attachments only while it fetches
/// that conversation, and an unchanged one is not fetched again. So after
/// the walk, every attachment that has not landed is tried again from the
/// conversation the store already holds.
async fn retry_attachments(
    client: &mut ChatGPTClient,
    db: &RawDb,
    opts: &FetchOptions,
    summary: &mut FetchSummary,
    blake3_by_file: &mut HashMap<String, String>,
    walk: &mut Walk,
) -> Result<()> {
    let pending: Vec<String> = db
        .conversations_with_unfetched_attachments()
        .await?
        .into_iter()
        .filter(|cid| !walk.attachments_tried.contains(cid))
        .collect();
    if pending.is_empty() {
        return Ok(());
    }
    info!(
        event = "chatgpt_attachment_retry",
        conversations = pending.len(),
        "trying again the attachments earlier runs did not land"
    );
    for cid in &pending {
        if opts.control.stop.requested() {
            break;
        }
        let Some(conv) = db.load_conversation_payload(cid).await? else {
            continue;
        };
        let attach =
            match fetch_attachments(client, &conv, summary, blake3_by_file, &opts.control.stop)
                .await
            {
                Ok(attach) => attach,
                Err(Cut::Stopped) => break,
                Err(Cut::RateLimited(reason)) => {
                    walk.rate_limited = true;
                    walk.found.phase(
                        "attachments",
                        format!(
                            "stopped at the rate limit; the rest is left for the next run: {reason}"
                        ),
                    );
                    break;
                }
            };
        flush_attachments(db, &attach).await?;
        if let Some(sealer) = opts.sealer.as_ref() {
            sealer.wrote(1).await;
        }
    }
    Ok(())
}

/// Top-level arrays the API returns as a *set*, in an order that varies
/// between fetches of an unchanged conversation. Sorted before the
/// write so that an unchanged record serializes identically to itself.
/// Every other array in the payload is ordered (`mapping.*.children`,
/// `content.parts`, citations, attachments) and must stay as sent.
const SET_VALUED_KEYS: &[&str] = &[
    "safe_urls",
    "blocked_urls",
    "disabled_tool_ids",
    "plugin_ids",
];

pub(crate) fn canonicalize_conversation_payload(payload: &Value) -> Value {
    let mut out = payload.clone();
    for key in SET_VALUED_KEYS {
        if let Some(bag) = out.get_mut(key).and_then(Value::as_array_mut) {
            // Sort by the rendered string so mixed types (which would make
            // `as_str` sort unstable) still get a total order.
            bag.sort_by_key(|v| {
                v.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| v.to_string())
            });
        }
    }
    out
}

fn title_and_update_time(full: &Value) -> (Option<String>, Option<String>) {
    let title = full.get("title").and_then(|v| v.as_str()).map(String::from);
    // `update_time` in the detail response is a Unix-epoch float, which
    // we store JSON-encoded ("1710959331.420159"). Note the *listing*
    // endpoint reports the same instant as an ISO-8601 string, so the
    // skip-check can't compare the stored text byte-for-byte against the
    // listing value — it canonicalizes both via `update_time_secs`.
    let update_time = full
        .get("update_time")
        .map(|v| serde_json::to_string(v).unwrap_or_default());
    (title, update_time)
}

fn update_time_secs(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_f64().map(|f| f.floor() as i64),
        Value::String(s) if !s.is_empty() => DateTime::parse_from_rfc3339(s)
            .ok()
            .map(|dt| dt.timestamp())
            // Tolerate a stringified epoch just in case the API ever
            // quotes the number.
            .or_else(|| s.parse::<f64>().ok().map(|f| f.floor() as i64)),
        _ => None,
    }
}

/// Canonicalize the *stored* column, which SQLite hands back as the
/// JSON-encoded text we wrote (`1710959331.420159` for a float,
/// `"…iso…"` for a string). Re-parse to recover the value's shape, then
/// reduce to seconds via [`update_time_secs`].
fn stored_update_time_secs(json_encoded: &str) -> Option<i64> {
    let v: Value = serde_json::from_str(json_encoded).ok()?;
    update_time_secs(&v)
}

/// Internal row shape used by [`upsert_conversations`] — same fields
/// `ConversationDetail` used to carry, before the migration to the
/// generic `bulk_upsert_in_tx` path.
#[derive(Debug, Clone)]
struct ConversationUpsert {
    id: String,
    title: Option<String>,
    update_time: Option<String>,
    payload: String,
}

async fn upsert_me(db: &RawDb, payload: &Value, now: &IsoOffsetTimestamp) -> Result<()> {
    let id = payload
        .get("id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow::anyhow!("/me response missing id"))?;
    let email = payload
        .get("email")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let name = payload
        .get("name")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let payload_str = serde_json::to_string(payload).context("serialize /me")?;
    let row = MeRow {
        id_and_payload: WirePayload {
            id: id.to_string(),
            payload: payload_str,
        },
        email,
        name,
    };
    let mut tx = db.pool().begin().await.context("begin upsert_me tx")?;
    bulk_upsert_in_tx(&mut tx, &[row], now).await?;
    tx.commit().await.context("commit upsert_me tx")?;
    Ok(())
}

/// Build a batch of `ConversationRow` values and bulk-upsert. Today
/// we still flush one-at-a-time because each detail fetch is its own
/// network round trip — but the path goes through the same shared
/// machinery every other ported provider uses.
async fn upsert_conversations(
    db: &RawDb,
    rows: &[ConversationUpsert],
    now: &IsoOffsetTimestamp,
) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let built: Vec<ConversationRowSchema> = rows
        .iter()
        .map(|r| ConversationRowSchema {
            id_and_payload: WirePayload {
                id: r.id.clone(),
                payload: r.payload.clone(),
            },
            title: r.title.clone(),
            update_time: r.update_time.clone(),
        })
        .collect();
    let mut tx = db
        .pool()
        .begin()
        .await
        .context("begin upsert_conversations tx")?;
    bulk_upsert_in_tx(&mut tx, &built, now).await?;
    tx.commit()
        .await
        .context("commit upsert_conversations tx")?;
    Ok(())
}

/// Every attachment and asset pointer a conversation's messages name, as
/// `(file_id, name, mime)`, each file once: identical assets often appear
/// under several parts (asset_pointer + attachments mirror).
fn attachment_targets(conv: &Value) -> Vec<(String, Option<String>, Option<String>)> {
    let Some(mapping) = conv.get("mapping").and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut targets: Vec<(String, Option<String>, Option<String>)> = Vec::new();
    for node in mapping.values() {
        let Some(msg) = node.get("message").and_then(|v| v.as_object()) else {
            continue;
        };
        if let Some(atts) = msg
            .get("metadata")
            .and_then(|m| m.get("attachments"))
            .and_then(|a| a.as_array())
        {
            for att in atts {
                let Some(id) = att.get("id").and_then(|v| v.as_str()) else {
                    continue;
                };
                if seen.insert(id.to_string()) {
                    let name = att
                        .get("name")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string());
                    let mime = att
                        .get("mime_type")
                        .or_else(|| att.get("mimeType"))
                        .and_then(|v| v.as_str())
                        .map(String::from);
                    targets.push((id.to_string(), name, mime));
                }
            }
        }
        if let Some(parts) = msg
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(|v| v.as_array())
        {
            for id in parts.iter().filter_map(image_asset_file_id) {
                if seen.insert(id.to_string()) {
                    targets.push((id.to_string(), None, Some("image/*".into())));
                }
            }
        }
    }
    targets
}

/// Pull every attachment + asset-pointer blob a conversation names. We
/// skip a file whose bytes we already have (signed URLs rotate; bytes
/// don't). A failure becomes the edge's `last_error` and does not fail
/// the sync; a file chatgpt.com no longer has is a warning the retry pass
/// leaves alone. A [`Cut`] when a stop or the give-up guard ended it:
/// what it fetched is dropped rather than recorded.
async fn fetch_attachments(
    client: &mut ChatGPTClient,
    conv: &Value,
    summary: &mut FetchSummary,
    blake3_by_file: &mut HashMap<String, String>,
    stop: &datalib_etl::stop::StopFlag,
) -> std::result::Result<CasEdgeAccumulator, Cut> {
    let mut attach = CasEdgeAccumulator::new();
    let Some(cid) = conv
        .get("conversation_id")
        .or_else(|| conv.get("id"))
        .and_then(|v| v.as_str())
    else {
        return Ok(attach);
    };
    for (file_id, name, mime) in attachment_targets(conv) {
        if let Some(blake3) = blake3_by_file.get(&file_id) {
            attach.add_known(cid, &file_id, blake3.clone());
            summary.skipped_blobs += 1;
            continue;
        }
        match download_one_file(client, &file_id, mime.as_deref()).await {
            Ok((bytes, content_type)) => {
                let blake3 = datalib_etl::blob_cas::blake3_hex(&bytes);
                blake3_by_file.insert(file_id.clone(), blake3);
                attach.add_fetched(cid, &file_id, bytes, content_type, name.clone());
                summary.new_blobs += 1;
            }
            Err(_) if stop.requested() => return Err(Cut::Stopped),
            Err(FileError::RateLimited(reason)) => return Err(Cut::RateLimited(reason)),
            Err(FileError::Gone(reason)) => {
                attach.add_skipped(cid, &file_id, Reason::NotFound, reason);
                summary.failed_blobs += 1;
            }
            Err(FileError::Failed(reason)) => {
                attach.add_failed(cid, &file_id, reason);
                summary.failed_blobs += 1;
            }
        }
    }
    Ok(attach)
}

async fn flush_attachments(db: &RawDb, attach: &CasEdgeAccumulator) -> Result<()> {
    attach
        .flush(db.pool(), db.cas(), |conv_id, file_id, blake3| {
            ConversationAttachmentRow {
                id: ConversationAttachmentRow::pk_recipe(conv_id, file_id),
                conversation_id: conv_id.to_string(),
                file_id: file_id.to_string(),
                blake3: blake3.map(String::from),
            }
        })
        .await
        .context("write a conversation's attachments")
}

/// The file id an `image_asset_pointer` content part points at, its
/// `sediment://` or `file-service://` scheme stripped; `None` for any
/// other part.
pub fn image_asset_file_id(part: &Value) -> Option<&str> {
    let obj = part.as_object()?;
    if obj.get("content_type").and_then(Value::as_str) != Some("image_asset_pointer") {
        return None;
    }
    let ptr = obj.get("asset_pointer").and_then(Value::as_str)?;
    Some(
        ptr.strip_prefix("sediment://")
            .or_else(|| ptr.strip_prefix("file-service://"))
            .unwrap_or(ptr),
    )
}

/// Fetch one attachment's bytes via the two-hop dance: metadata via
/// latchkey (auth attached), then `latchkey curl -fSL` on the signed
/// URL (no auth — Azure rejects the chatgpt cookie). The error says why
/// there are no bytes; it becomes the edge's problem.
async fn download_one_file(
    client: &mut ChatGPTClient,
    file_id: &str,
    mime: Option<&str>,
) -> std::result::Result<(Vec<u8>, Option<String>), FileError> {
    let meta = match client
        .get(&format!("/backend-api/files/{file_id}/download"))
        .await
    {
        Ok(meta) => meta,
        Err(ChatGPTError::RateLimited { reason, .. }) => {
            return Err(FileError::RateLimited(reason))
        }
        Err(ChatGPTError::Permanent(msg))
            if msg.contains("HTTP 404") || msg.contains("HTTP 410") =>
        {
            return Err(FileError::Gone(format!("file metadata: {msg}")))
        }
        Err(e) => return Err(FileError::Failed(format!("file metadata: {e}"))),
    };
    download_signed(client, &meta, file_id, mime)
        .await
        .map_err(|e| FileError::Failed(format!("{e:#}")))
}

/// Why a file has no bytes; the text becomes the edge's problem.
enum FileError {
    /// chatgpt.com no longer has it. Trying again cannot help until the
    /// conversation changes, and a refetch of it tries its files again.
    Gone(String),
    /// The give-up guard tripped.
    RateLimited(String),
    /// Anything else, which the next run tries again.
    Failed(String),
}

/// The second hop: the bytes behind the signed URL the metadata names.
async fn download_signed(
    client: &ChatGPTClient,
    meta: &Value,
    file_id: &str,
    mime: Option<&str>,
) -> Result<(Vec<u8>, Option<String>)> {
    let signed = match meta.get("download_url").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => anyhow::bail!("the file's metadata carries no download URL"),
    };

    // Step 2: signed-URL GET via latchkey shim. We write to a tempfile
    // and slurp the bytes — keeps the existing curl shellout shape
    // (which uses `-o <path>`) and side-steps any binary-stdio
    // weirdness. The tempfile is deleted automatically.
    let tmp = tempfile::NamedTempFile::new().context("create blob tempfile")?;
    // The signed CDN URL is CF-fronted; mark the request so the router
    // curl routes it to the impersonating curl. The helper supplies
    // `[--account <acct>] curl`, so the blob fetch runs as the same
    // identity as the API calls that discovered it.
    let mut cmd = latchkey_curl_command(client.latchkey())?;
    cmd.arg("-fSL")
        .arg("-H")
        .arg(IMPERSONATE_MARKER_HEADER)
        .arg("-o")
        .arg(tmp.path())
        .arg(&signed);
    let proc = tokio::time::timeout(ATTACH_FILE_TIMEOUT, cmd.output())
        .await
        .context("file curl timed out")?
        .context("file curl spawn failed")?;
    if !proc.status.success() {
        let stderr_full = String::from_utf8_lossy(&proc.stderr).into_owned();
        let tail: String = stderr_full
            .chars()
            .rev()
            .take(200)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        anyhow::bail!(
            "signed-URL download exit {}: {}",
            proc.status.code().unwrap_or(-1),
            tail.trim()
        );
    }
    let bytes =
        std::fs::read(tmp.path()).with_context(|| format!("read tempfile for {file_id}"))?;
    Ok((bytes, mime.map(String::from)))
}

/// A conversation listing, and whether it is the *whole* listing.
///
/// The distinction is the only thing that makes pruning safe here. The walk
/// stops early for two ordinary reasons — a `since` cutoff, a `max_pages`
/// cap — and in both cases the pages it never asked for hold conversations
/// that still exist. Deleting on that reading would remove most of the
/// archive on the first `since`-scoped run.
struct Listing {
    items: Vec<Value>,
    /// True only when the walk ran out of conversations rather than out of
    /// permission to look: it reached `total`, or a page's `items` came
    /// back an empty array.
    complete: bool,
    /// The page that failed and why. The pages before it are kept.
    failed: Option<String>,
    /// The page failed because the give-up guard tripped.
    rate_limited: bool,
}

#[instrument(skip_all, fields(max_pages, since_secs))]
async fn list_all_conversations(
    client: &mut ChatGPTClient,
    max_pages: Option<usize>,
    since_secs: Option<i64>,
    progress: &datalib_etl::progress::Progress,
) -> Listing {
    let mut items: Vec<Value> = Vec::new();
    let mut offset = 0usize;
    let mut pages = 0usize;
    let mut complete = false;
    let mut failed = None;
    let mut rate_limited = false;
    loop {
        let page = match client.list_conversations_page(offset, PAGE_SIZE).await {
            Ok(page) => page,
            Err(e) => {
                rate_limited = matches!(e, ChatGPTError::RateLimited { .. });
                failed = Some(format!("page at offset {offset}: {e}"));
                break;
            }
        };
        let page_items = match page_items(&page) {
            Ok(page_items) => page_items,
            Err(e) => {
                failed = Some(format!("page at offset {offset}: {e}"));
                break;
            }
        };
        let total = page.get("total").and_then(|v| v.as_u64());
        info!(
            event = "chatgpt_listing_page",
            offset = offset,
            got = page_items.len(),
            total = total.unwrap_or(0),
            cum = items.len() + page_items.len(),
            "listed one page of conversations"
        );
        let got = page_items.len();
        items.extend(page_items);
        // The listing is `order=updated` (newest first), so once a
        // page *ends* older than the `since` cutoff every later page
        // is older still — stop walking. The page's own items are kept
        // (any below the cutoff classify as out-of-scope); an
        // unparseable `update_time` never stops the walk.
        let page_ends_before_since = match (since_secs, items.last()) {
            (Some(cutoff), Some(last)) => last
                .get("update_time")
                .and_then(update_time_secs)
                .is_some_and(|secs| secs < cutoff),
            _ => false,
        };
        offset += got;
        pages += 1;
        progress.set_message(&format!("listing page {pages}, {} convs", items.len()));
        if got == 0 {
            complete = true;
            break;
        }
        if page_ends_before_since {
            info!(
                event = "chatgpt_listing_since_stop",
                pages = pages,
                "the listing reached what the last run already had; stopping"
            );
            break;
        }
        if let Some(t) = total {
            if offset as u64 >= t {
                complete = true;
                break;
            }
        }
        if let Some(cap) = max_pages {
            if pages >= cap {
                info!(
                    event = "chatgpt_listing_capped",
                    max_pages = cap,
                    "the listing stopped at the configured page cap"
                );
                break;
            }
        }
        sleep(SLEEP_BETWEEN).await;
    }
    Listing {
        items,
        complete,
        failed,
        rate_limited,
    }
}

/// A 200 that is not a listing page is no evidence the listing ended:
/// read as an empty page it would make the walk complete and prune
/// everything the pages before it did not name.
fn page_items(page: &Value) -> std::result::Result<Vec<Value>, String> {
    match page.get("items") {
        Some(Value::Array(items)) => Ok(items.clone()),
        _ => {
            let preview: String = page.to_string().chars().take(120).collect();
            Err(format!("answered with no `items` array: {preview}"))
        }
    }
}

/// Parse a `since` config value — full RFC 3339 or bare `YYYY-MM-DD`
/// (assumed UTC midnight) — down to the whole-second Unix epoch the
/// skip-check compares at. Same accepted forms as slack's `since` and
/// claude's.
fn parse_since_secs(s: &str) -> Result<i64> {
    let t = datalib_time::parse_strict(s)
        .or_else(|_| datalib_time::parse_yyyy_mm_dd_assumed_utc(s))
        .with_context(|| format!("expected RFC 3339 or YYYY-MM-DD, got {s:?}"))?;
    Ok(t.inner().timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn iso_for_epoch(epoch: f64) -> String {
        let micros = (epoch * 1_000_000.0).round() as i64;
        DateTime::from_timestamp_micros(micros)
            .unwrap()
            .format("%Y-%m-%dT%H:%M:%S%.6f+00:00")
            .to_string()
    }

    #[test]
    fn update_time_secs_matches_across_listing_and_detail_shapes() {
        // The exact bug this guards: the detail endpoint hands back a
        // Unix-epoch float, the listing endpoint the same instant as an
        // ISO-8601 string. Both must canonicalize to the same second.
        let epoch = 1_710_959_331.420159_f64;
        let detail = json!(epoch);
        let listing = json!(iso_for_epoch(epoch));
        assert_eq!(update_time_secs(&detail), Some(1_710_959_331));
        assert_eq!(update_time_secs(&detail), update_time_secs(&listing));
    }

    #[test]
    fn stored_update_time_secs_reparses_json_encoded_text() {
        // The column comes back as the JSON text we wrote. Float and ISO
        // encodings of the same instant must reduce to the same second.
        let epoch = 1_710_959_331.420159_f64;
        let float_text = serde_json::to_string(&json!(epoch)).unwrap();
        let iso_text = serde_json::to_string(&json!(iso_for_epoch(epoch))).unwrap();
        assert_eq!(stored_update_time_secs(&float_text), Some(1_710_959_331));
        assert_eq!(
            stored_update_time_secs(&float_text),
            stored_update_time_secs(&iso_text)
        );
        // Sub-second drift between the two endpoints is collapsed away.
        let jittered = serde_json::to_string(&json!(epoch + 0.4)).unwrap();
        assert_eq!(
            stored_update_time_secs(&jittered),
            stored_update_time_secs(&float_text)
        );
        // Unparseable text is `None` → caller treats the row as stale.
        assert_eq!(stored_update_time_secs("garbage"), None);
        assert_eq!(update_time_secs(&Value::Null), None);
    }

    #[test]
    fn parse_since_secs_accepts_date_and_rfc3339() {
        // 2024-03-20T00:00:00Z
        assert_eq!(parse_since_secs("2024-03-20").unwrap(), 1_710_892_800);
        assert_eq!(
            parse_since_secs("2024-03-20T18:28:51Z").unwrap(),
            1_710_959_331
        );
        // Offsets are honored: 18:28:51+02:00 is 16:28:51Z.
        assert_eq!(
            parse_since_secs("2024-03-20T18:28:51+02:00").unwrap(),
            1_710_952_131
        );
        assert!(parse_since_secs("not-a-date").is_err());
    }

    #[test]
    fn since_cutoff_compares_at_seconds_grain_with_listing_shape() {
        // The scope filter compares `update_time_secs(listing item)`
        // against the parsed cutoff — same grain as the skip-check, so
        // a listing ISO string on the cutoff second is in scope.
        let cutoff = parse_since_secs("2024-03-20T18:28:51Z").unwrap();
        let on_boundary = json!(iso_for_epoch(1_710_959_331.9));
        let just_before = json!(iso_for_epoch(1_710_959_330.1));
        assert!(update_time_secs(&on_boundary).unwrap() >= cutoff);
        assert!(update_time_secs(&just_before).unwrap() < cutoff);
    }

    // ── unordered bags from the API ──────────────────────────────────

    /// Two fetches of an unchanged conversation must serialize
    /// identically; the API returns `safe_urls` in a varying order.
    #[test]
    fn safe_urls_are_stored_in_a_stable_order() {
        let one = json!({
            "conversation_id": "c1",
            "safe_urls": ["https://openai.com", "https://chatgpt.com"],
            "blocked_urls": ["https://b.example", "https://a.example"],
        });
        let other_order = json!({
            "conversation_id": "c1",
            "safe_urls": ["https://chatgpt.com", "https://openai.com"],
            "blocked_urls": ["https://a.example", "https://b.example"],
        });
        assert_eq!(
            canonicalize_conversation_payload(&one),
            canonicalize_conversation_payload(&other_order),
            "the same urls in a different order must canonicalize the same"
        );
    }

    /// Sorting, not dropping: a url that goes away is a real change and
    /// must still show up as one.
    #[test]
    fn safe_urls_keep_their_contents() {
        let full = json!({"safe_urls": ["b", "a", "c"]});
        let fewer = json!({"safe_urls": ["a", "b"]});
        assert_eq!(
            canonicalize_conversation_payload(&full)["safe_urls"],
            json!(["a", "b", "c"]),
            "every url is kept, in sorted order"
        );
        assert_ne!(
            canonicalize_conversation_payload(&full),
            canonicalize_conversation_payload(&fewer),
            "a removed url must still read as a change"
        );
    }

    /// The ordered arrays must not be touched: `children` is branch
    /// order and `parts` is reading order.
    #[test]
    fn canonicalize_leaves_ordered_arrays_alone() {
        let payload = json!({
            "mapping": {"root": {"children": ["m2", "m1"], "message": {"content": {"parts": ["z", "a"]}}}},
        });
        assert_eq!(canonicalize_conversation_payload(&payload), payload);
    }

    /// A conversation without the field, with `null` in it (the API
    /// sends `"plugin_ids": null`), or with something unexpected, passes
    /// through rather than panicking — this runs on every conversation
    /// of every sync.
    #[test]
    fn canonicalize_tolerates_a_missing_or_odd_field() {
        let none = json!({"conversation_id": "c1"});
        assert_eq!(canonicalize_conversation_payload(&none), none);
        let null = json!({"plugin_ids": null});
        assert_eq!(canonicalize_conversation_payload(&null), null);
        let odd = json!({"safe_urls": "not-an-array"});
        assert_eq!(canonicalize_conversation_payload(&odd), odd);
        let mixed = json!({"safe_urls": [2, "a", 1]});
        assert_eq!(
            canonicalize_conversation_payload(&mixed)["safe_urls"],
            json!([1, 2, "a"]),
            "mixed types still get a total order rather than panicking"
        );
    }
}
