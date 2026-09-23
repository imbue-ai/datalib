//! Gmail REST API downloader — the third mode of `type: email`.

pub mod api;
pub mod ingest;

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use datalib_etl::blob_cas::{CasEdgeAccumulator, CasEdgeRow as _};
use datalib_etl::bulk::bulk_upsert_entity_in_tx;
use datalib_etl::control::DownloadControl;
use datalib_etl::download_problems::{self, DownloadProblem};
use datalib_etl::download_run::DownloadRun;
use datalib_etl::http::LatchkeySettings;
use datalib_etl::progress::{Progress, RunBar};
use datalib_etl::scope_config::{self, FilterChange};
use datalib_time::IsoOffsetTimestamp;
use serde::Serialize;
use serde_json::{json, Value};
use tracing::{info, warn};

use datalib_etl_email_config::EmailGmailApi;

use super::db::RawDb;
use super::schema_raw::{EmlBlobRow, GmailMessageRow, ThreadRow};
use super::K_ONLY_EXTRACT_LABELS;
use api::{Client, QuotaThrottle};
use ingest::LabelIndex;

/// `messages.list` page size. Google's maximum is 500; ids are tiny, so
/// there is no reason to ask for less.
const LIST_PAGE_SIZE: u32 = 500;
/// Flush accumulated rows every this many messages, so an interrupted run
/// keeps what it already fetched and peak memory stays bounded.
const FLUSH_BATCH: usize = 200;

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// The store this run writes into, opened and closed by the caller.
    /// A download never opens a store of its own: two live connections to
    /// one `.doltlite_db` make each other's `dolt_commit` fail. See
    /// `datalib/backend/etl/README.md`.
    pub db: RawDb,
    /// Seals a flushed batch; see `RunState::sealer`.
    pub sealer: Option<datalib_etl::raw_store::Sealer>,
    pub config: EmailGmailApi,
    /// Which latchkey identity the download authenticates as, from the
    /// source's `latchkey_settings:` block. `google-gmail` routinely holds
    /// both a work and a personal account, which is the case this setting
    /// was introduced for.
    pub latchkey: LatchkeySettings,
    /// When non-empty, only ingest messages carrying at least one label
    /// whose canonical path exactly matches one of these.
    pub only_labels: Vec<String>,
    pub blob_size_limit_bytes: Option<u64>,
    pub progress: Progress,
    pub control: DownloadControl,
}

impl FetchOptions {
    /// Every field defaulted except the store, which has none to give:
    /// it is a live handle the caller opens and closes.
    pub fn new(db: RawDb) -> Self {
        Self {
            db,
            sealer: None,
            config: EmailGmailApi::default(),
            latchkey: LatchkeySettings::default(),
            only_labels: Vec::new(),
            blob_size_limit_bytes: None,
            progress: Progress::noop(),
            control: DownloadControl::default(),
        }
    }
}

#[derive(Debug, Default, Serialize, Clone)]
pub struct FetchSummary {
    pub mailboxes_upserted: usize,
    pub threads_upserted: usize,
    pub emails_upserted: usize,
    pub emails_destroyed: usize,
    pub blobs_stored: usize,
    pub blobs_skipped: usize,
    pub blobs_oversize: usize,
    pub messages_filtered: usize,
    /// Ids `history.list` or `messages.list` named that we already had —
    /// skipped before spending any quota on them.
    pub messages_already_had: usize,
    /// Gmail quota units spent, against the per-minute ceiling.
    pub quota_units_spent: u64,
    /// True when the run stopped at `message_budget` with more to fetch.
    /// A partial backfill is a successful outcome, not a failure.
    pub budget_exhausted: bool,
    /// True when the step was asked to stop with more to fetch. Holds the
    /// cursor exactly as `budget_exhausted` does.
    pub interrupted: bool,
    /// True when there was no usable cursor — none stored, `full_resync`
    /// set, or one that aged out — and the whole filter was re-enumerated.
    pub full_sync: bool,
    /// Labels enumerated on top of the history replay because
    /// `only_extract_labels` widened since the cursor was stored: the
    /// newly-admitted label names, or `["*"]` when the filter was removed.
    /// `history.list` cannot surface mail that merely *existed* outside
    /// the old filter, so a widening needs its own walk.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub backfilled_labels: Vec<String>,
    /// Messages the enumeration named that `messages.get` would not
    /// return, for a reason other than the message being gone. Holds the
    /// cursor: see where it is written.
    pub messages_failed: usize,
    /// Configured labels this account does not have. Reported rather
    /// than fatal: one misspelling costs that label, not the run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<DownloadProblem>,
}

fn state_scope(account_id: &str) -> String {
    format!("gmail:{account_id}:historyId")
}

/// Scope key for this mode's [`scope_config`] blob. Prefixed `gmail:`, the
/// namespace `RawDb::reset` clears.
const SCOPE_CONFIG_KEY: &str = "gmail:download";

/// The config that decides which mail lands on disk, recorded beside the
/// cursor so the next run can tell a widened filter from an unchanged
/// one. Only the label filter qualifies: `message_budget` is a per-run
/// budget and `full_resync` a one-off override.
fn scope_config_blob(opts: &FetchOptions) -> Value {
    let mut labels: Vec<&str> = opts.only_labels.iter().map(String::as_str).collect();
    labels.sort_unstable();
    json!({ K_ONLY_EXTRACT_LABELS: labels })
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db = opts.db.clone();

    // Stamp a `sync_runs` row for this pass, the same as every other
    // live source. It is what the DAG-level run-2 incrementality golden
    // reads: a source with no row there is reported as file-backed, so
    // skipping this would make a Gmail incrementality regression
    // invisible to the golden whose whole job is catching one.
    let run = DownloadRun::start(
        db.pool(),
        &json!({
            "user_id": opts.config.user_id(),
            "account": opts.latchkey.account(),
            "full_resync": opts.config.full_resync,
            "only_extract_labels": opts.only_labels,
            "message_budget": opts.config.message_budget,
        }),
    )
    .await?;

    let scope_cfg = scope_config_blob(&opts);
    let prior_scope_cfg = scope_config::load_or_none(db.pool(), SCOPE_CONFIG_KEY).await;
    let label_change = scope_config::filter_widened(
        prior_scope_cfg.as_ref(),
        K_ONLY_EXTRACT_LABELS,
        &opts.only_labels,
    );

    let result = run_sync(&db, &opts, &label_change).await;
    // Even on error, record a summary stub so the row has the same
    // fields a successful one does — the defaults populated as far as
    // the run got. Mirrors the JMAP path.
    let summary_for_bookkeeping = result.as_ref().cloned().unwrap_or_default();
    // Record the filter only once a run has mirrored everything under
    // it. A run that held its cursor (budget, or a failed fetch) has
    // not, and the next run must plan the same backfill again.
    let satisfied = result.as_ref().is_ok_and(|s| s.drained());
    scope_config::store_if_satisfied(db.pool(), SCOPE_CONFIG_KEY, &scope_cfg, satisfied).await;
    run.finish(&result, &summary_for_bookkeeping).await;
    result
}

impl FetchSummary {
    /// Whether the run did every fetch it set out to do. The cursor and
    /// the recorded filter both advance only on a drained run: storing
    /// either after a partial one tells the next run "caught up", and an
    /// incremental run never re-lists what this one skipped.
    pub fn drained(&self) -> bool {
        !self.stopped_early() && self.messages_failed == 0
    }

    /// The run ended before the walk did, on purpose.
    pub fn stopped_early(&self) -> bool {
        self.budget_exhausted || self.interrupted
    }
}

async fn run_sync(
    db: &RawDb,
    opts: &FetchOptions,
    label_change: &FilterChange,
) -> Result<FetchSummary> {
    let cfg = &opts.config;
    let user_id = cfg.user_id().to_string();

    let mut throttle = QuotaThrottle::new(cfg.quota_units_per_minute());
    let client = throttle.client(opts.latchkey.clone());
    let mut summary = FetchSummary::default();
    let now = IsoOffsetTimestamp::now_local();

    // ── account ─────────────────────────────────────────────────────
    throttle.acquire(api::UNITS_GET_PROFILE).await;
    let profile = api::get_profile(&user_id, &client)
        .await
        .context("users.getProfile — is `latchkey auth browser google-gmail` done?")?;
    let account_id = cfg
        .account_id
        .clone()
        .unwrap_or_else(|| profile.email_address.clone());
    let email_address = cfg
        .email_address
        .clone()
        .unwrap_or_else(|| profile.email_address.clone());
    let display_name = cfg
        .display_name
        .clone()
        .unwrap_or_else(|| account_id.clone());
    super::upsert_account(
        db,
        &now,
        &account_id,
        &json!({
            "id": account_id,
            "name": display_name,
            "email": email_address,
            "isPersonal": true,
            "_source": { "via": "gmail.googleapis.com" },
        }),
    )
    .await?;

    // ── labels → mailboxes ──────────────────────────────────────────
    throttle.acquire(api::UNITS_LABELS_LIST).await;
    let index = LabelIndex::new(api::list_labels(&user_id, &client).await?);
    let mailbox_payloads: Vec<Value> = index
        .mailboxes(&account_id)
        .into_iter()
        .map(|(id, name, role)| json!({ "id": id, "name": name, "role": role }))
        .collect();
    super::upsert_mailboxes(db, &now, &account_id, &mailbox_payloads).await?;
    summary.mailboxes_upserted = mailbox_payloads.len();
    info!(
        event = "gmail_labels",
        account = %account_id,
        labels = mailbox_payloads.len(),
        "listed the account's labels",
    );

    // Turn `only_extract_labels` into Gmail label ids so the enumeration
    // is narrowed server-side. Doing it client-side would mean paying
    // `messages.get`'s 20 quota units for every message in the account
    // to keep a handful — see `api::list_messages`.
    let resolved = index.ids_for_names(&opts.only_labels)?;
    let filter_label_ids = resolved.resolved;
    summary.problems = resolved.problems;
    download_problems::report(db.pool(), &summary.problems).await;
    if !opts.only_labels.is_empty() {
        info!(
            event = "gmail_label_filter",
            labels = %opts.only_labels.join(", "),
            ids = %filter_label_ids.join(", "),
            unresolved = summary.problems.len(),
            "restricting enumeration server-side",
        );
    }

    // ── decide what to replay and what to walk ──────────────────────
    let stored = if cfg.full_resync {
        None
    } else {
        db.load_scope(&state_scope(&account_id)).await?
    };
    let history = match &stored {
        None => None,
        Some(cursor) => {
            throttle.acquire(api::UNITS_HISTORY_LIST).await;
            match collect_history(&user_id, &client, cursor, &mut throttle).await {
                Ok(changes) => Some(changes),
                // Only `history.list` reads a 404 this way.
                Err(e) if is_not_found(&e) => {
                    warn!(
                        event = "gmail_history_expired",
                        account = %account_id,
                        cursor = %cursor,
                        "stored historyId aged out of Google's retention window; re-enumerating",
                    );
                    None
                }
                Err(e) => return Err(e),
            }
        }
    };
    let plan = plan_walk(history, label_change, &filter_label_ids, &index);
    info!(
        event = "gmail_plan",
        account = %account_id,
        stored_cursor = stored.as_deref(),
        full_resync = cfg.full_resync,
        label_change = label_change.as_str(),
        history = plan.history.as_ref().map(|c| c.added.len() + c.relabeled.len() + c.deleted.len()),
        walk = plan.walk.as_deref().map(describe_walk),
        plan = %plan.describe(),
        "planned the walk"
    );

    // ── fetch ───────────────────────────────────────────────────────
    // Loaded once per run, not once per page: both are whole-table reads
    // and `fetch_ids` is called per `messages.list` page.
    let known_blobs = db.loaded_blob_ids().await?;
    let known_gmail_ids = load_known_gmail_ids(db).await?;

    let mut state = RunState {
        db,
        sealer: opts.sealer.as_ref(),
        index: &index,
        account_id: &account_id,
        user_id: &user_id,
        client: &client,
        now: &now,
        only_labels: opts.only_labels.iter().cloned().collect(),
        blob_size_limit_bytes: opts.blob_size_limit_bytes,
        budget: opts.config.message_budget,
        fetched: 0,
        known_blobs,
        known_gmail_ids,
        threads: BTreeSet::new(),
        // Nothing fixed to seed it with: unlike the JMAP path, this one
        // has no coarse phase ticks, so the bar stays at 0/0 until the
        // history replay or the first `messages.list` page names a size.
        bar: RunBar::new(&opts.progress, 0),
        pending: Pending::default(),
    };

    // The cursor to store *if* the run gets through its work. Sampled
    // before any walk, so anything that changed while it ran is replayed
    // next run rather than missed.
    let next_cursor: Option<String> = match &plan.history {
        None => profile.history_id.clone(),
        Some(changes) => changes.history_id.clone(),
    };

    if let Some(changes) = &plan.history {
        summary.emails_destroyed = destroy(db, &changes.deleted).await?;
        let ids: Vec<String> = changes
            .added
            .iter()
            .chain(changes.relabeled.iter())
            .cloned()
            .collect();
        info!(
            event = "gmail_history_replay",
            account = %account_id,
            since = stored.as_deref().unwrap_or(""),
            fetch = ids.len(),
            deleted = changes.deleted.len(),
            "replaying history since the stored cursor",
        );
        // The id list is materialized, so this stretch has an exact size.
        state.bar.expect(ids.len() as u64);
        state.bar.doing("replaying history");
        fetch_ids(&mut state, &mut throttle, &ids, opts, &mut summary).await?;
    } else {
        summary.full_sync = true;
    }

    if let Some(walk_label_ids) = &plan.walk {
        if plan.history.is_some() {
            summary.backfilled_labels = plan.backfilled_labels.clone();
        }
        let enumerated = if summary.stopped_early() {
            None
        } else {
            full_sync(
                &mut state,
                &mut throttle,
                opts,
                walk_label_ids,
                &mut summary,
            )
            .await?
        };
        // A walk that covered the whole mailbox is the one moment this
        // provider can see a deletion `history.list` never reported —
        // one outside its retention window, or outside the old filter.
        //
        // Two conditions, both about whether the walk was authoritative
        // over the whole mailbox. A label filter narrows it server-side,
        // so messages outside those labels are unlisted rather than
        // deleted; a budget-limited walk never asked for its remaining
        // pages. Either one makes absence meaningless.
        match (&enumerated, walk_label_ids.is_empty()) {
            (Some(seen), true) => {
                summary.emails_destroyed += prune_to_enumeration(db, seen).await?;
            }
            _ => info!(
                event = "gmail_prune_skipped",
                label_filtered = !walk_label_ids.is_empty(),
                budget_exhausted = summary.budget_exhausted,
                "the walk was not authoritative over the whole mailbox; \
                 not treating unlisted messages as deleted",
            ),
        }
    }

    flush(&mut state, &mut summary).await?;
    flush_threads(&mut state, &mut summary).await?;
    state.bar.finish();

    // Only advance the cursor when the run drained its work, and a run
    // has two ways not to: it stopped at `message_budget`, or a
    // `messages.get` failed for a reason other than the message being
    // gone. Either way storing the cursor tells the next run "you are
    // caught up" — and because the next run is then incremental,
    // `history.list` only names what *changed*, so a message that merely
    // failed to fetch is never named again. It would be missing until
    // the cursor aged out or someone set `full_resync`.
    //
    // Leaving the cursor put means the next run re-enumerates — cheap,
    // because `messages.list` is 5 units a page and every id already
    // fetched is skipped before spending `messages.get`'s 20.
    if !summary.drained() {
        info!(
            event = "gmail_cursor_held",
            fetched = summary.emails_upserted,
            failed = summary.messages_failed,
            budget_exhausted = summary.budget_exhausted,
            interrupted = summary.interrupted,
            "work this run did not do; leaving the cursor so the next run resumes",
        );
    } else if let Some(h) = &next_cursor {
        db.save_scope(&state_scope(&account_id), h).await?;
        info!(event = "gmail_cursor_stored", cursor = %h, "stored the historyId cursor");
    }

    summary.quota_units_spent = throttle.spent_total();
    info!(
        event = "gmail_summary",
        account = %account_id,
        emails_upserted = summary.emails_upserted,
        emails_destroyed = summary.emails_destroyed,
        threads_upserted = summary.threads_upserted,
        blobs_stored = summary.blobs_stored,
        blobs_skipped = summary.blobs_skipped,
        blobs_oversize = summary.blobs_oversize,
        messages_already_had = summary.messages_already_had,
        messages_filtered = summary.messages_filtered,
        messages_failed = summary.messages_failed,
        quota_units_spent = summary.quota_units_spent,
        full_sync = summary.full_sync,
        backfilled_labels = %summary.backfilled_labels.join(", "),
        "gmail sync finished",
    );
    Ok(summary)
}

/// What one run does: replay `history.list` since the stored cursor, walk
/// `messages.list`, or both.
///
/// No cursor means one walk over the configured filter. A cursor whose
/// filter has not moved means the replay alone. A cursor whose filter
/// *widened* needs both: the replay for what changed under the old
/// labels, and a walk over what is newly in scope — the mail that was
/// already there under those labels never appears in `history.list`,
/// because nothing about it changed.
struct Plan {
    history: Option<Changes>,
    /// Label ids to walk; an empty list is one unrestricted walk. `None`
    /// walks nothing.
    walk: Option<Vec<String>>,
    /// What `walk` is for, as configured names, when it is a backfill.
    backfilled_labels: Vec<String>,
}

impl Plan {
    fn describe(&self) -> String {
        match (&self.history, &self.walk) {
            (None, Some(ids)) => format!("full sync: {}", describe_walk(ids)),
            (Some(_), None) => "incremental: history replay only".to_string(),
            (Some(_), Some(ids)) => format!(
                "incremental, and the label filter widened: history replay plus a backfill \
                 walk over {}",
                describe_walk(ids)
            ),
            (None, None) => "nothing to do".to_string(),
        }
    }
}

fn describe_walk(label_ids: &[String]) -> String {
    if label_ids.is_empty() {
        "the whole account".to_string()
    } else {
        format!("labels {label_ids:?}")
    }
}

fn plan_walk(
    history: Option<Changes>,
    label_change: &FilterChange,
    filter_label_ids: &[String],
    index: &LabelIndex,
) -> Plan {
    let no_backfill = |history| Plan {
        history,
        walk: None,
        backfilled_labels: Vec::new(),
    };
    if history.is_none() {
        return Plan {
            history,
            walk: Some(filter_label_ids.to_vec()),
            backfilled_labels: Vec::new(),
        };
    }
    match label_change {
        FilterChange::Unchanged => no_backfill(history),
        FilterChange::WidenedToAll => Plan {
            history,
            walk: Some(Vec::new()),
            backfilled_labels: vec!["*".to_string()],
        },
        FilterChange::Added(names) => {
            // A name that resolves to nothing was already reported as a
            // `download_problem` when the whole filter was resolved.
            let ids: Vec<String> = names.iter().filter_map(|n| index.id_for_name(n)).collect();
            if ids.is_empty() {
                return no_backfill(history);
            }
            Plan {
                history,
                walk: Some(ids),
                backfilled_labels: names.clone(),
            }
        }
    }
}

#[derive(Debug, Default)]
struct Changes {
    added: Vec<String>,
    relabeled: Vec<String>,
    deleted: Vec<String>,
    history_id: Option<String>,
}

fn is_not_found(e: &anyhow::Error) -> bool {
    e.downcast_ref::<api::GmailApiError>()
        .is_some_and(|e| matches!(e, api::GmailApiError::NotFound))
}

async fn collect_history(
    user_id: &str,
    client: &Client,
    cursor: &str,
    throttle: &mut QuotaThrottle,
) -> Result<Changes> {
    let mut out = Changes::default();
    let mut token: Option<String> = None;
    loop {
        let page = api::list_history(user_id, client, cursor, token.as_deref()).await?;
        out.added.extend(page.added);
        out.relabeled.extend(page.relabeled);
        out.deleted.extend(page.deleted);
        if let Some(h) = page.history_id {
            out.history_id = Some(h);
        }
        match page.next_page_token {
            Some(t) => {
                throttle.acquire(api::UNITS_HISTORY_LIST).await;
                token = Some(t);
            }
            None => break,
        }
    }
    // An id can appear in several pages; fetching it twice is wasted quota.
    out.added.sort();
    out.added.dedup();
    out.relabeled.sort();
    out.relabeled.dedup();
    out.relabeled.retain(|id| !out.added.contains(id));
    out.deleted.sort();
    out.deleted.dedup();
    out.added.retain(|id| !out.deleted.contains(id));
    out.relabeled.retain(|id| !out.deleted.contains(id));
    Ok(out)
}

struct RunState<'a> {
    db: &'a RawDb,
    /// Seals a flushed batch, so render can start on the mail already
    /// mirrored while the walk continues. `None` commits once at the end.
    sealer: Option<&'a datalib_etl::raw_store::Sealer>,
    index: &'a LabelIndex,
    account_id: &'a str,
    user_id: &'a str,
    client: &'a Client,
    now: &'a IsoOffsetTimestamp,
    /// Belt-and-braces client-side label check. The enumeration is
    /// already narrowed server-side; this catches the case where a
    /// configured label name matched no Gmail label at all, so the
    /// server-side filter was empty and would otherwise mean "everything".
    only_labels: BTreeSet<String>,
    blob_size_limit_bytes: Option<u64>,
    budget: Option<usize>,
    fetched: usize,
    /// CAS keys already on disk, loaded once per run.
    known_blobs: std::collections::HashMap<String, String>,
    /// Gmail ids already mirrored, loaded once per run. Skipping these
    /// before spending `messages.get` is what makes a budget-limited
    /// backfill make progress across runs instead of re-fetching the
    /// same prefix forever.
    known_gmail_ids: BTreeSet<String>,
    /// Thread ids touched this run; membership is rebuilt from the
    /// `emails` table at the end, not from what this run happened to see.
    threads: BTreeSet<String>,
    /// The run's one progress bar. Announcing a total is what turns the
    /// Manage screen's activity cell from a bare count into "N queued"
    /// ticking down; each phase adds what it has learned it will do.
    bar: RunBar,
    pending: Pending,
}

#[derive(Default)]
struct Pending {
    emails: Vec<super::schema_raw::EmailRow>,
    gmail_ids: Vec<GmailMessageRow>,
    cas: CasEdgeAccumulator,
    seen_blob_ids: BTreeSet<String>,
}

/// Walk every message id Gmail will name, fetching the ones we lack.
///
/// Returns the ids the walk saw, or `None` when the walk did not finish —
/// it stopped at `message_budget`. The distinction is what makes pruning
/// safe: a budget-limited walk has pages it never asked for, and the
/// messages in them still exist.
async fn full_sync(
    state: &mut RunState<'_>,
    throttle: &mut QuotaThrottle,
    opts: &FetchOptions,
    label_ids: &[String],
    summary: &mut FetchSummary,
) -> Result<Option<BTreeSet<String>>> {
    let mut seen: BTreeSet<String> = BTreeSet::new();
    for label_id in enumeration_walks(label_ids) {
        let label = label_id.map_or_else(|| "<all>".to_string(), |id| state.index.name(id));
        let mut token: Option<String> = None;
        let mut pages = 0usize;
        let mut listed = 0usize;
        state.bar.doing(&label);
        // Everything the run committed to before this walk. Gmail's
        // estimate is of this walk alone, and there is one walk per
        // configured label, so each walk's size adds to the run's.
        let before = state.bar.announced();
        loop {
            throttle.acquire(api::UNITS_MESSAGES_LIST).await;
            let page = api::list_messages(
                state.user_id,
                state.client,
                token.as_deref(),
                LIST_PAGE_SIZE,
                label_id,
            )
            .await?;
            pages += 1;
            listed += page.ids.len();
            // The estimate can come in under what the walk really lists,
            // and a total below that reads as 0 remaining mid-walk.
            let want = page.result_size_estimate.unwrap_or(0).max(listed as u64);
            state.bar.expect_at_least(before + want);
            // A message under two configured labels is listed by both
            // walks; `seen` is what keeps the second listing free.
            let fresh: Vec<String> = page
                .ids
                .into_iter()
                .filter(|id| seen.insert(id.clone()))
                .collect();
            let fetched_before = state.fetched;
            fetch_ids(state, throttle, &fresh, opts, summary).await?;
            info!(
                event = "gmail_list_page",
                label = %label,
                page = pages,
                listed = listed,
                fresh = fresh.len(),
                fetched = state.fetched - fetched_before,
                fetched_total = state.fetched,
                already_had = summary.messages_already_had,
                more = page.next_page_token.is_some(),
                "walked one messages.list page",
            );
            if summary.stopped_early() {
                return Ok(None);
            }
            match page.next_page_token {
                Some(t) => token = Some(t),
                None => break,
            }
        }
        info!(
            event = "gmail_walk_done",
            label = %label,
            pages = pages,
            listed = listed,
            "finished walking one label",
        );
    }
    Ok(Some(seen))
}

// `only_extract_labels` means "carrying **any** of these", and Gmail's
// `messages.list` cannot express that in one request: repeated `labelIds`
// intersect. So one walk per label, unioned here. No labels configured is
// one unrestricted walk.
fn enumeration_walks(label_ids: &[String]) -> Vec<Option<&str>> {
    if label_ids.is_empty() {
        vec![None]
    } else {
        label_ids.iter().map(|id| Some(id.as_str())).collect()
    }
}

async fn fetch_ids(
    state: &mut RunState<'_>,
    throttle: &mut QuotaThrottle,
    ids: &[String],
    opts: &FetchOptions,
    summary: &mut FetchSummary,
) -> Result<()> {
    for id in ids {
        // Already mirrored: skip before spending 20 quota units on it.
        // This is what lets successive budget-limited runs walk forward
        // through a large mailbox instead of re-fetching the same prefix.
        if state.known_gmail_ids.contains(id) {
            summary.messages_already_had += 1;
            // Ticked although nothing was fetched: the total counts ids
            // *listed*, and a re-walk of a mirrored mailbox is almost
            // all skips, which would otherwise never move the bar.
            state.bar.did(1);
            continue;
        }
        // Asked to stop: the same partial result a spent budget gives —
        // what was flushed is sealed, the cursor is held, the next run
        // resumes.
        if opts.control.stop.requested() {
            summary.interrupted = true;
            info!(
                event = "gmail_interrupted",
                fetched = state.fetched,
                "stopping with a partial result; the cursor is held so the next run resumes",
            );
            return Ok(());
        }
        if state.budget.is_some_and(|b| state.fetched >= b) {
            summary.budget_exhausted = true;
            info!(
                event = "gmail_budget_exhausted",
                fetched = state.fetched,
                "stopping early with a partial result; the cursor is held so the next run resumes",
            );
            return Ok(());
        }
        throttle.acquire(api::UNITS_MESSAGES_GET).await;
        let msg = match api::get_message_raw(state.user_id, state.client, id).await {
            Ok(m) => m,
            Err(e) if is_not_found(&e) => {
                // Deleted between the list and the get: normal on a busy
                // mailbox, and nothing to come back for.
                info!(event = "gmail_message_deleted_before_fetch", id = %id, "a listed message was gone before it could be fetched");
                state.bar.did(1);
                continue;
            }
            // The retry loop backed off for as long as the run's give-up
            // bounds allow and Google still would not serve. Walking on
            // would fail every remaining id the same way, one attempt
            // each; stopping keeps what the sealed batches already
            // committed, and the held cursor makes the next run resume.
            Err(e) if api::is_gave_up(&e) => {
                return Err(e.context(format!("fetching message {id}")));
            }
            Err(e) => {
                // Not a deletion, so this message still exists and we
                // still want it. Counted, and the count holds the cursor.
                warn!(event = "gmail_message_failed", id = %id, error = %e, "a message could not be fetched");
                summary.messages_failed += 1;
                state.bar.did(1);
                continue;
            }
        };
        state.fetched += 1;
        state.bar.did(1);

        let ingested = match ingest::ingest(state.account_id, state.index, &msg) {
            Ok(i) => i,
            Err(e) => {
                warn!(event = "gmail_ingest_failed", id = %msg.id, error = %e, "a message could not be stored");
                continue;
            }
        };

        // Extract-time label filter, on the same canonical paths every
        // other mode matches against.
        if !state.only_labels.is_empty()
            && !ingested
                .label_paths
                .iter()
                .any(|p| state.only_labels.contains(p))
        {
            summary.messages_filtered += 1;
            continue;
        }

        let oversize = state
            .blob_size_limit_bytes
            .is_some_and(|cap| ingested.raw.len() as u64 > cap);
        if oversize {
            summary.blobs_oversize += 1;
        } else if state.known_blobs.contains_key(&ingested.blob_id)
            || state.pending.seen_blob_ids.contains(&ingested.blob_id)
        {
            summary.blobs_skipped += 1;
        } else {
            state.pending.seen_blob_ids.insert(ingested.blob_id.clone());
            state.pending.cas.add_fetched(
                &ingested.email_id,
                &ingested.blob_id,
                ingested.raw.clone(),
                Some("message/rfc822".to_string()),
                None,
            );
            summary.blobs_stored += 1;
        }

        state.threads.insert(ingested.thread_id.clone());
        state.known_gmail_ids.insert(msg.id.clone());
        state.pending.gmail_ids.push(GmailMessageRow {
            gmail_id: msg.id.clone(),
            email_id: ingested.email_id.clone(),
            thread_id: ingested.thread_id.clone(),
        });
        state.pending.emails.push(ingested.row);

        if state.pending.emails.len() >= FLUSH_BATCH {
            flush(state, summary).await?;
        }
    }
    Ok(())
}

async fn flush(state: &mut RunState<'_>, summary: &mut FetchSummary) -> Result<()> {
    // The CAS accumulator has no emptiness predicate, but it only ever
    // gains entries alongside an email row, so the row count answers for
    // both.
    if state.pending.emails.is_empty() {
        return Ok(());
    }
    let rows = std::mem::take(&mut state.pending.emails);
    summary.emails_upserted += rows.len();
    super::upsert_emails(state.db, state.now, &rows).await?;

    // The Gmail-id → row mapping, in the same run so a crash between the
    // two can only ever lose the mapping (recovered by re-fetching), not
    // strand a row that nothing can find.
    let ids = std::mem::take(&mut state.pending.gmail_ids);
    if !ids.is_empty() {
        let mut tx = state.db.pool().begin().await.context("begin gmail id tx")?;
        // `_entity_in_tx`, not `bulk_upsert_in_tx`: this table has no
        // bookkeeping sidecar. It is derived bookkeeping itself, with no
        // upstream payload to retry or diff — and pairing it with a
        // sidecar would double its row count for nothing.
        bulk_upsert_entity_in_tx(&mut tx, &ids).await?;
        tx.commit().await.context("commit gmail id tx")?;
    }
    let cas = std::mem::take(&mut state.pending.cas);
    // CAS bytes + `email_blobs` edges + edge bookkeeping, through the
    // same shared primitive every other provider's blob pass uses.
    cas.flush(
        state.db.pool(),
        state.db.cas(),
        |email_id, blob_id, blake3| EmlBlobRow {
            id: EmlBlobRow::pk_recipe(email_id, blob_id),
            email_id: email_id.to_string(),
            blob_id: blob_id.to_string(),
            blake3: blake3.map(str::to_string),
        },
    )
    .await?;
    // End of a flush is the consistent point: the email rows, their Gmail-id
    // mapping and their blob bytes all landed above, and nothing here is
    // mid-prune -- `prune_to_enumeration` runs after the walk, and only when
    // the walk was authoritative over the whole mailbox.
    if let Some(sealer) = state.sealer {
        sealer.wrote(1).await;
    }
    Ok(())
}

async fn flush_threads(state: &mut RunState<'_>, summary: &mut FetchSummary) -> Result<()> {
    let threads = std::mem::take(&mut state.threads);
    if threads.is_empty() {
        return Ok(());
    }
    let mut rows = Vec::with_capacity(threads.len());
    for thread_id in threads {
        let members: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT id, received_at FROM emails WHERE thread_id = ? AND account_id = ?",
        )
        .bind(&thread_id)
        .bind(state.account_id)
        .fetch_all(state.db.pool())
        .await
        .with_context(|| format!("reading membership of thread {thread_id}"))?;
        if members.is_empty() {
            continue;
        }
        let mut members = members;
        // Stable order for render: by receipt time, then id to break ties.
        members.sort_by(|a, b| {
            a.1.as_deref()
                .unwrap_or("")
                .cmp(b.1.as_deref().unwrap_or(""))
                .then_with(|| a.0.cmp(&b.0))
        });
        let email_ids: Vec<Value> = members
            .into_iter()
            .map(|(id, _)| Value::String(id))
            .collect();
        rows.push(ThreadRow::from_jmap_payload(
            &thread_id,
            state.account_id,
            &json!({ "id": thread_id, "emailIds": email_ids }),
        )?);
    }
    summary.threads_upserted = rows.len();
    super::upsert_threads(state.db, state.now, &rows).await
}

async fn destroy(db: &RawDb, gmail_ids: &[String]) -> Result<usize> {
    if gmail_ids.is_empty() {
        return Ok(0);
    }
    let mut email_ids = Vec::new();
    for id in gmail_ids {
        let email_id: Option<String> =
            sqlx::query_scalar("SELECT email_id FROM gmail_messages WHERE gmail_id = ?")
                .bind(id)
                .fetch_optional(db.pool())
                .await
                .with_context(|| format!("looking up Gmail message {id}"))?;
        // Not ours to delete: Gmail reported a message we never mirrored
        // (filtered out by label, or deleted before we ever saw it).
        let Some(email_id) = email_id else { continue };
        email_ids.push(email_id);
        sqlx::query("DELETE FROM gmail_messages WHERE gmail_id = ?")
            .bind(id)
            .execute(db.pool())
            .await
            .with_context(|| format!("clearing the mapping for {id}"))?;
    }
    // The same cascade the JMAP path's tombstones take. Deleting only
    // `emails` leaves this message's mailbox and keyword joins, its blob
    // refs and both sidecars behind, pointing at a row that no longer
    // exists.
    db.delete_emails(&email_ids).await?;
    Ok(email_ids.len())
}

/// Delete every mirrored Gmail message the enumeration did not name.
///
/// Callers must have established that the walk covered the whole mailbox —
/// see the gate at the callsite. That gate carries the whole argument: a
/// token that quietly lost a scope lists far fewer messages rather than
/// failing, and the only thing standing between that and a large prune is
/// whether the enumeration was authoritative.
async fn prune_to_enumeration(db: &RawDb, seen: &BTreeSet<String>) -> Result<usize> {
    let held = load_known_gmail_ids(db).await?;
    let gone: Vec<String> = held.difference(seen).cloned().collect();
    if gone.is_empty() {
        return Ok(0);
    }
    let n = destroy(db, &gone).await?;
    datalib_etl::prune::record("gmail messages", held.len(), n);
    Ok(n)
}

async fn load_known_gmail_ids(db: &RawDb) -> Result<BTreeSet<String>> {
    let ids: Vec<String> = sqlx::query_scalar("SELECT gmail_id FROM gmail_messages")
        .fetch_all(db.pool())
        .await
        .context("loading known Gmail message ids")?;
    Ok(ids.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cursors are namespaced per account so two Gmail mirrors in one raw
    /// store don't overwrite each other — same discipline as the JMAP
    /// path's `jmap:` keys.
    #[test]
    fn namespaces_the_cursor_per_account() {
        assert_eq!(
            state_scope("thad@imbue.com"),
            "gmail:thad@imbue.com:historyId"
        );
        assert_ne!(state_scope("a@x"), state_scope("b@x"));
        // Must not collide with the JMAP path's keys in the same table.
        assert!(state_scope("a@x").starts_with("gmail:"));
    }

    /// Three configured labels are three enumerations. Asking for them
    /// in one request returns the intersection, which for most label sets
    /// is empty — the mirror then downloads nothing and reports success.
    #[test]
    fn walks_each_configured_label_separately() {
        let labels = vec![
            "INBOX".to_string(),
            "STARRED".to_string(),
            "L_7".to_string(),
        ];
        assert_eq!(
            enumeration_walks(&labels),
            vec![Some("INBOX"), Some("STARRED"), Some("L_7")],
        );
    }

    /// No filter is one walk that names no label — not zero walks, which
    /// would mirror nothing at all.
    #[test]
    fn walks_the_whole_mailbox_when_no_label_is_configured() {
        assert_eq!(enumeration_walks(&[]), vec![None]);
    }

    /// A 404 has to be recognized through anyhow's context chain — it
    /// arrives wrapped. Both callers depend on this: `history.list`
    /// reads it as an expired cursor, `messages.get` as a deleted
    /// message, and everything else is a failure worth holding the
    /// cursor for.
    #[test]
    fn recognizes_a_404_through_context() {
        let e = anyhow::Error::new(api::GmailApiError::NotFound)
            .context("users.history.list")
            .context("while syncing");
        assert!(is_not_found(&e));
        assert!(!is_not_found(&anyhow::anyhow!("some other failure")));
    }
}
