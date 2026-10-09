//! ChatGPT downloader: the account's conversations, as
//! `/backend-api/conversations` lists them newest-updated-first and
//! held at the `update_time` it names; each one's attachments are edges
//! its row lists, owed until their bytes land. Both are fetched through
//! `datalib_etl_web::owed`, which owns the stop, the failure budget, the
//! flush and what each outcome means for a record. Nothing is marked
//! done: holding the content at the listed version is done
//! (docs/dev/data_architecture_ingestion.md, "What is left to fetch").

pub mod api;
pub mod db;
mod fetchers;
pub mod schema_raw;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{Context, Result};
use chrono::DateTime;
use datalib_etl::bulk::bulk_upsert_in_tx;
use datalib_etl::doltlite_raw::{self as dr, WirePayload};
use datalib_etl::download_problems::DownloadProblem;
use datalib_etl::download_run::DownloadRun;
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl::stop::StopFlag;
use datalib_etl_web::http::LatchkeySettings;
use datalib_etl_web::owed::{self, Fetcher, Listed, Loop};
use datalib_time::IsoOffsetTimestamp;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::time::sleep;
use tracing::{info, info_span, instrument, Instrument};

pub use api::{ChatGPTClient, ChatGPTError};
pub use db::{db_path_for, Conversation, FileRef, LoadedConversation, LoadedRaw, RawDb};
use fetchers::{Attachments, Conversations};
use schema_raw::{MeRow, ATTACHMENTS, CONVERSATIONS};

/// Between listing pages. ChatGPT doesn't appear to throttle us at any
/// polite rate; 100ms keeps us from looking like a tight loop.
pub const SLEEP_BETWEEN: Duration = Duration::from_millis(100);
pub const PAGE_SIZE: usize = 100;

/// Requests in a row that came to nothing before a loop gives up on
/// this run. A rate limit ends the run on its own; this ends it on an
/// answer the retry guard does not retry, such as a `401` on every
/// conversation from an expired token.
const FAILURE_BUDGET: usize = 25;

/// Timeout for one attachment's bytes through the latchkey shim.
pub(crate) const ATTACH_FILE_TIMEOUT: Duration = Duration::from_secs(600);

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
    /// Seals as flushes land, when the step driver hands one over.
    pub sealer: Option<datalib_etl::raw_store::Sealer>,
    pub max_pages: Option<usize>,
    /// Conversations fetched per run, at most.
    pub limit: Option<usize>,
    /// Between detail fetches.
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
    /// The run-pinned `--now`, which stamps the `me` row; `None`
    /// samples the clock.
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
    /// Listed, in scope, and already held at the version listed.
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
    /// Edges satisfied from bytes the CAS already held.
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
    let _ = datalib_etl_web::latchkey::ensure_curl_router();
    let db = opts.db.clone();
    let run_config = json!({
        "max_pages": opts.max_pages,
        "limit": opts.limit,
        "since": opts.since,
        "conv_uuids": opts.conv_uuids,
    });
    let run = DownloadRun::start(db.pool(), &run_config).await?;
    let client = ChatGPTClient::with_latchkey(opts.latchkey.clone());
    let ctx = Ctx {
        client: &client,
        db: &db,
        opts: &opts,
        found: &found,
        files: Mutex::new(HashMap::new()),
        config_problems: Mutex::new(Vec::new()),
        counts: Counts::default(),
    };
    let mut summary = FetchSummary::default();
    let result = phases(&ctx, &mut summary).await;
    summary.requests = client.requests();
    summary.network_seconds = client.network_seconds();
    summary.new_blobs = ctx.counts.new_blobs.load(Ordering::Relaxed);
    summary.skipped_blobs = ctx.counts.skipped_blobs.load(Ordering::Relaxed);
    run.finish(&result, &summary).await;
    result?;
    Ok(summary)
}

/// What this run's fetchers count as they go.
#[derive(Default)]
pub(crate) struct Counts {
    pub new_blobs: AtomicUsize,
    pub skipped_blobs: AtomicUsize,
}

/// What the listing and every loop of one run share.
pub(crate) struct Ctx<'a> {
    pub client: &'a ChatGPTClient,
    pub db: &'a RawDb,
    pub opts: &'a FetchOptions,
    pub found: &'a RunProblems,
    /// Per conversation fetched this run, the files it names: what the
    /// attachment loop asks for without reading the row back.
    pub files: Mutex<HashMap<String, Vec<FileRef>>>,
    /// The named conversations chatgpt.com has not got.
    pub config_problems: Mutex<Vec<DownloadProblem>>,
    pub counts: Counts,
}

impl Ctx<'_> {
    pub fn stop(&self) -> &StopFlag {
        &self.opts.control.stop
    }

    async fn wrote(&self, rows: u64) {
        if let Some(sealer) = &self.opts.sealer {
            sealer.wrote(rows).await;
        }
    }

    pub fn remember_files(&self, c: &Conversation) {
        self.files
            .lock()
            .unwrap()
            .insert(c.id.clone(), c.files.clone());
    }

    /// What the conversation says of one of its files: from this run's
    /// fetch of it, or the stored row. `None` when it names no such file.
    pub async fn file_ref(&self, conv_id: &str, file_id: &str) -> Result<Option<FileRef>> {
        let known = self
            .files
            .lock()
            .unwrap()
            .get(conv_id)
            .map(|files| files.iter().find(|f| f.id == file_id).cloned());
        if let Some(found) = known {
            return Ok(found);
        }
        let Some(payload) = self.db.load_conversation_payload(conv_id).await? else {
            return Ok(None);
        };
        Ok(attachment_targets(&payload)
            .into_iter()
            .find(|f| f.id == file_id))
    }

    /// Runs one loop over `owed`; whether a rate limit ended it, which
    /// every later request would meet too.
    async fn drain<T: Send>(
        &self,
        table: &'static str,
        phase: &str,
        owed: Vec<Listed>,
        f: &impl Fetcher<T>,
        flush: usize,
    ) -> Result<owed::Drained> {
        let l = Loop {
            pool: self.db.pool(),
            table,
            phase,
            stop: self.stop(),
            found: self.found,
            sealer: self.opts.sealer.as_ref(),
            batch: 1,
            concurrency: 1,
            flush,
            flush_bytes: 32 << 20,
            failures_in_a_row: FAILURE_BUDGET,
        };
        owed::drain(&l, owed, f).await
    }

    /// `conv_uuids`: exactly the named conversations, each fetched every
    /// run, since no listing says whether one has moved. One chatgpt.com
    /// has not got is a `config:` row; the rest are held at the version
    /// their detail names, so a later listing run leaves them alone.
    async fn named(&self, s: &mut FetchSummary) -> Result<bool> {
        let named = &self.opts.conv_uuids;
        self.opts.progress.set_length(Some(named.len() as u64));
        for (i, raw) in named.iter().enumerate() {
            if self.stop().requested() {
                break;
            }
            self.opts.progress.inc(1);
            self.opts.progress.set_message(raw);
            let target = datalib_etl::ids::normalize_id_token(raw);
            match self.client.get_conversation(&target).await {
                Ok(full) => {
                    let c = parse_conversation(&target, &full)?;
                    let version = version_of(full.get("update_time"));
                    let mut tx = self.db.pool().begin().await?;
                    self.db.store_conversation(&mut tx, &c).await?;
                    owed::hold(&mut tx, CONVERSATIONS, &target, version.as_deref()).await?;
                    tx.commit().await?;
                    self.remember_files(&c);
                    s.fetched += 1;
                    self.wrote(1).await;
                    info!(event = "chatgpt_fetch_single_ok", raw = raw, id = %target, "fetched one conversation by id");
                }
                // The transport refused it because of the stop.
                Err(_) if self.stop().requested() => break,
                Err(ChatGPTError::RateLimited { reason, .. }) => {
                    self.found.phase(
                        "conversations",
                        format!(
                            "rate-limited after {} fetched; {} left for the next run: {reason}",
                            s.fetched,
                            named.len() - i
                        ),
                    );
                    return Ok(true);
                }
                Err(ChatGPTError::Permanent(msg)) if msg.contains("HTTP 404") => {
                    self.config_problems
                        .lock()
                        .unwrap()
                        .push(DownloadProblem::not_found(
                            "conv_uuids",
                            raw,
                            format!("chatgpt.com has no conversation with this id: {msg}"),
                        ));
                }
                Err(ChatGPTError::Permanent(msg)) => {
                    let mut tx = self.db.pool().begin().await?;
                    dr::record_object_error(&mut tx, CONVERSATIONS, &target, &msg).await?;
                    tx.commit().await?;
                    s.errors += 1;
                }
            }
        }
        Ok(false)
    }

    /// The listing walk, the prune a complete one allows, then every
    /// listed conversation the store does not hold at the version
    /// listed, the never-fetched first. Whether a rate limit ended it.
    async fn listed(&self, s: &mut FetchSummary, since_secs: Option<i64>) -> Result<bool> {
        self.opts.progress.set_message("listing conversations");
        let Listing {
            items,
            complete,
            failed,
            rate_limited,
        } = list_all_conversations(
            self.client,
            self.opts.max_pages,
            since_secs,
            &self.opts.progress,
        )
        .instrument(info_span!("chatgpt_list"))
        .await;
        if let Some(e) = failed {
            if self.stop().requested() {
                return Ok(false);
            }
            // With nothing listed and nothing stored there is nothing to
            // fall back on: the run did nothing at all.
            if items.is_empty() && !self.db.has_any_conversation().await? {
                return Err(anyhow::anyhow!("list conversations: {e}"));
            }
            self.found.listing("conversations", e);
            // Every detail fetch would be refused the same way.
            if rate_limited {
                return Ok(true);
            }
        }
        info!(
            event = "chatgpt_listing",
            convs = items.len(),
            complete,
            "listed the conversations"
        );
        s.listing = items.len();

        // A complete walk is an authoritative census of the account, so a
        // conversation we hold that it did not name has been deleted on
        // chatgpt.com. An incomplete one says nothing: the pages it never
        // asked for are full of conversations that still exist. With
        // `since` configured most runs stop early and prune nothing.
        if complete {
            let keep: HashSet<String> = items
                .iter()
                .filter_map(|c| c.get("id").and_then(Value::as_str))
                .map(String::from)
                .collect();
            s.pruned = self.db.prune_conversations(&keep).await?;
        }

        // `since` gates fetching only: rows already stored stay, so moving
        // it further back later lists the older conversations as owed. An
        // unparseable `update_time` is in scope.
        let mut listed: Vec<Listed> = Vec::new();
        for item in &items {
            let Some(id) = item.get("id").and_then(Value::as_str) else {
                continue;
            };
            let update_time = item.get("update_time");
            if let (Some(cutoff), Some(secs)) = (since_secs, update_time.and_then(update_time_secs))
            {
                if secs < cutoff {
                    s.out_of_scope += 1;
                    continue;
                }
            }
            listed.push(Listed::new(id, version_of(update_time)));
        }
        let in_scope = listed.len();
        let mut owed = owed_missing_first(self.db.pool(), CONVERSATIONS, listed).await?;
        s.skipped = in_scope - owed.len();
        if let Some(limit) = self.opts.limit {
            owed.truncate(limit);
        }
        info!(
            event = "chatgpt_priority_split",
            owed = owed.len(),
            up_to_date = s.skipped,
            out_of_scope = s.out_of_scope,
            "sorted the listing into what to fetch"
        );
        self.opts.progress.set_length(Some(owed.len() as u64));
        let drained = self
            // One conversation per transaction: each is one slow request,
            // and a seal may follow every one, so a long first sync
            // reaches the grid as it goes.
            .drain(
                CONVERSATIONS,
                "conversations",
                owed,
                &Conversations(self),
                1,
            )
            .await?;
        s.fetched = drained.got;
        s.errors = drained.failed;
        Ok(drained.terminal.is_some())
    }

    /// Every attachment edge the store lists and does not hold at its
    /// conversation's version. Whether a rate limit ended it.
    async fn attachments(&self, s: &mut FetchSummary) -> Result<bool> {
        let listed = self.db.attachments_listed().await?;
        let owed = owed::owed(self.db.pool(), ATTACHMENTS, listed).await?;
        self.opts.progress.set_message("attachments");
        let drained = self
            .drain(ATTACHMENTS, "attachments", owed, &Attachments(self), 8)
            .await?;
        s.failed_blobs = drained.failed;
        Ok(drained.terminal.is_some())
    }
}

/// `/me`, the conversations, then their attachments. A rate limit
/// anywhere ends the run's requests: the loop it struck leaves a
/// `phase:` row, and the `config:` rows stand, since not every named
/// conversation was checked.
async fn phases(ctx: &Ctx<'_>, s: &mut FetchSummary) -> Result<()> {
    let me = ctx
        .client
        .me()
        .await
        .map_err(|e| anyhow::anyhow!("fetch /me: {e}"))?;
    upsert_me(ctx, &me).await?;
    info!(
        event = "chatgpt_me",
        email = me.get("email").and_then(|v| v.as_str()).unwrap_or(""),
        id = me.get("id").and_then(|v| v.as_str()).unwrap_or(""),
        "signed in as this account"
    );
    // Canonicalized to whole-second epoch, the grain the listing's
    // version is kept at (see `version_of`).
    let since_secs = ctx
        .opts
        .since
        .as_deref()
        .map(parse_since_secs)
        .transpose()
        .with_context(|| format!("sync.since {:?}", ctx.opts.since))?;
    let mut rate_limited = if ctx.opts.conv_uuids.is_empty() {
        ctx.listed(s, since_secs).await?
    } else {
        ctx.named(s).await?
    };
    if !rate_limited && !ctx.stop().requested() {
        rate_limited = ctx.attachments(s).await?;
    }
    if rate_limited {
        ctx.found.cut_short();
    } else {
        ctx.found
            .config(std::mem::take(&mut *ctx.config_problems.lock().unwrap()));
    }
    Ok(())
}

/// Of `listed`, what `table` does not hold at the version listed: the
/// records never fetched first, then the stale, so a run cut short
/// spent its budget on new work.
async fn owed_missing_first(
    pool: &sqlx::SqlitePool,
    table: &str,
    listed: Vec<Listed>,
) -> Result<Vec<Listed>> {
    let held = owed::held_versions(pool, table, listed.iter().map(|l| l.key.as_str())).await?;
    let (missing, stale): (Vec<Listed>, Vec<Listed>) = listed
        .into_iter()
        .filter(|l| !held.get(&l.key).is_some_and(|h| h.satisfies(&l.version)))
        .partition(|l| !held.get(&l.key).is_some_and(|h| h.fetched));
    Ok(missing.into_iter().chain(stale).collect())
}

async fn upsert_me(ctx: &Ctx<'_>, payload: &Value) -> Result<()> {
    let id = payload
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("/me response missing id"))?;
    let now = match &ctx.opts.now {
        Some(s) => datalib_time::parse_strict(s).context("--now")?,
        None => IsoOffsetTimestamp::now_local(),
    };
    let row = MeRow {
        id_and_payload: WirePayload {
            id: id.to_string(),
            payload: serde_json::to_string(payload).context("serialize /me")?,
        },
        email: payload
            .get("email")
            .and_then(Value::as_str)
            .map(String::from),
        name: payload
            .get("name")
            .and_then(Value::as_str)
            .map(String::from),
    };
    let mut tx = ctx.db.pool().begin().await.context("begin upsert_me tx")?;
    bulk_upsert_in_tx(&mut tx, &[row], &now).await?;
    tx.commit().await.context("commit upsert_me tx")?;
    Ok(())
}

/// One conversation as the detail endpoint answered it, ready to store.
pub(crate) fn parse_conversation(id: &str, full: &Value) -> Result<Conversation> {
    let full = canonicalize_conversation_payload(full);
    Ok(Conversation {
        id: id.to_string(),
        title: full.get("title").and_then(Value::as_str).map(String::from),
        // The detail's `update_time` is a Unix-epoch float, stored
        // JSON-encoded ("1710959331.420159").
        update_time: full
            .get("update_time")
            .map(|v| serde_json::to_string(v).unwrap_or_default()),
        files: attachment_targets(&full),
        payload: serde_json::to_string(&full).context("serialize conversation")?,
    })
}

/// The version a conversation is listed at: its `update_time` in whole
/// seconds, which the listing's ISO-8601 string and the detail's epoch
/// float both reduce to. One that will not parse is kept as written, so
/// the record is fetched again when it changes rather than every run.
pub(crate) fn version_of(update_time: Option<&Value>) -> Option<String> {
    let v = update_time?;
    update_time_secs(v)
        .map(|s| s.to_string())
        .or_else(|| v.as_str().map(String::from))
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

/// Every attachment and asset pointer a conversation's messages name,
/// each file once.
pub(crate) fn attachment_targets(conv: &Value) -> Vec<FileRef> {
    let Some(mapping) = conv.get("mapping").and_then(Value::as_object) else {
        return Vec::new();
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut targets: Vec<FileRef> = Vec::new();
    for node in mapping.values() {
        let Some(msg) = node.get("message").and_then(Value::as_object) else {
            continue;
        };
        if let Some(atts) = msg
            .get("metadata")
            .and_then(|m| m.get("attachments"))
            .and_then(Value::as_array)
        {
            for att in atts {
                let Some(id) = att.get("id").and_then(Value::as_str) else {
                    continue;
                };
                if seen.insert(id.to_string()) {
                    targets.push(FileRef {
                        id: id.to_string(),
                        name: att.get("name").and_then(Value::as_str).map(String::from),
                        mime: att
                            .get("mime_type")
                            .or_else(|| att.get("mimeType"))
                            .and_then(Value::as_str)
                            .map(String::from),
                    });
                }
            }
        }
        if let Some(parts) = msg
            .get("content")
            .and_then(|c| c.get("parts"))
            .and_then(Value::as_array)
        {
            for id in parts.iter().filter_map(image_asset_file_id) {
                if seen.insert(id.to_string()) {
                    targets.push(FileRef {
                        id: id.to_string(),
                        name: None,
                        mime: Some("image/*".into()),
                    });
                }
            }
        }
    }
    targets
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
    client: &ChatGPTClient,
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
        let total = page.get("total").and_then(Value::as_u64);
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
/// listing's version is kept at. Same accepted forms as slack's `since`
/// and claude's.
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

    /// The version a conversation is held at is the same whether the
    /// listing or the detail named it, sub-second drift between the two
    /// collapsed away; a stamp that will not parse is kept as written.
    #[test]
    fn a_version_is_the_same_from_the_listing_and_the_detail() {
        let epoch = 1_710_959_331.420159_f64;
        assert_eq!(
            version_of(Some(&json!(epoch))).as_deref(),
            Some("1710959331")
        );
        assert_eq!(
            version_of(Some(&json!(iso_for_epoch(epoch)))),
            version_of(Some(&json!(epoch + 0.4)))
        );
        assert_eq!(
            version_of(Some(&json!("garbage"))).as_deref(),
            Some("garbage")
        );
        assert_eq!(version_of(None), None);
        assert_eq!(version_of(Some(&Value::Null)), None);
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
        // against the parsed cutoff — same grain as the version, so a
        // listing ISO string on the cutoff second is in scope.
        let cutoff = parse_since_secs("2024-03-20T18:28:51Z").unwrap();
        let on_boundary = json!(iso_for_epoch(1_710_959_331.9));
        let just_before = json!(iso_for_epoch(1_710_959_330.1));
        assert!(update_time_secs(&on_boundary).unwrap() >= cutoff);
        assert!(update_time_secs(&just_before).unwrap() < cutoff);
    }

    /// Each file once, with what the message says of it.
    #[test]
    fn attachment_targets_name_each_file_once() {
        let conv = json!({"mapping": {
            "n1": {"message": {"metadata": {"attachments": [
                {"id": "f-1", "name": "a.txt", "mime_type": "text/plain"},
                {"id": "f-1", "name": "a.txt", "mime_type": "text/plain"},
            ]}, "content": {"parts": [
                {"content_type": "image_asset_pointer", "asset_pointer": "file-service://f-2"},
            ]}}},
        }});
        let ids: Vec<(String, Option<String>)> = attachment_targets(&conv)
            .into_iter()
            .map(|f| (f.id, f.mime))
            .collect();
        assert_eq!(
            ids,
            [
                ("f-1".to_string(), Some("text/plain".to_string())),
                ("f-2".to_string(), Some("image/*".to_string()))
            ]
        );
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
