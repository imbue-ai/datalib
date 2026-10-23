//! Gmail REST API downloader — the third mode of `type: email`.
//!
//! `history.list` or a `messages.list` walk says which messages exist
//! and which changed; `super::listed` keeps that apart from what has
//! been fetched, and `messages.get` is asked for whatever the store owes.

pub mod api;
pub mod ingest;

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};

use anyhow::{Context, Result};
use async_trait::async_trait;
use datalib_etl::blob_cas::CasInsert;
use datalib_etl::bulk::{bulk_upsert_bookkeeping, BulkUpsertable as _};
use datalib_etl::control::DownloadControl;
use datalib_etl::doltlite_raw as dr;
use datalib_etl::download_problems::DownloadProblem;
use datalib_etl::download_run::DownloadRun;
use datalib_etl::progress::{Progress, RunBar};
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl::stop::StopFlag;
use datalib_etl_web::http::LatchkeySettings;
use datalib_etl_web::owed::{self, BatchError, Fetched, Fetcher, Listed, Outcome};
use datalib_time::IsoOffsetTimestamp;
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::{Sqlite, Transaction};
use tracing::{info, warn};

use datalib_etl_email_config::EmailGmailApi;

use super::db::{write_eml_edges_in_tx, RawDb};
use super::envelope::GmailId;
use super::labels;
use super::listed::{self, Named, WHOLE_ACCOUNT};
use super::mbox;
use super::schema_raw::{EmailRow, EmlBlobRow};
use api::{Client, GmailMessage, QuotaThrottle};
use ingest::{Ingested, LabelIndex};

/// `messages.list` page size. Google's maximum is 500; ids are tiny, so
/// there is no reason to ask for less.
const LIST_PAGE_SIZE: u32 = 500;
/// Fetched messages written in one transaction, and the bytes of raw
/// messages that force a write sooner: they wait in memory until then.
const FLUSH_BATCH: usize = 200;
const FLUSH_BYTES: usize = 32 * 1024 * 1024;

/// The phase name a `problems` row carries when the fetch gave up.
const P_MESSAGES_GET: &str = "messages.get";

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// The store this run writes into, opened and closed by the caller.
    /// A download never opens a store of its own: one writer per file
    /// (`datalib/backend/etl/README.md` § "One writer per file, by
    /// construction").
    pub db: RawDb,
    /// Seals a flushed batch, so render can start on the mail already
    /// mirrored while the fetch continues. `None` commits once at the end.
    pub sealer: Option<datalib_etl::raw_store::Sealer>,
    pub config: EmailGmailApi,
    /// Which latchkey identity the download authenticates as, from the
    /// source's `latchkey_settings:` block. `google-gmail` routinely holds
    /// both a work and a personal account, which is the case this setting
    /// was introduced for.
    pub latchkey: LatchkeySettings,
    /// A Google Takeout of this account to start from: its messages land
    /// before the API is asked, so the API sends their labels and not
    /// the messages again ([`mbox::seed`]).
    pub takeout: Option<PathBuf>,
    /// When non-empty, only ingest messages carrying at least one label
    /// whose canonical path exactly matches one of these.
    pub only_labels: Vec<String>,
    pub blob_size_limit_bytes: Option<u64>,
    /// Messages written in one transaction. `None` is [`FLUSH_BATCH`].
    pub flush_batch: Option<usize>,
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
            takeout: None,
            only_labels: Vec::new(),
            blob_size_limit_bytes: None,
            flush_batch: None,
            progress: Progress::noop(),
            control: DownloadControl::default(),
        }
    }
}

#[derive(Debug, Default, Serialize, Clone)]
pub struct FetchSummary {
    pub mailboxes_upserted: usize,
    /// Labels Gmail no longer has, whose rows went.
    pub mailboxes_destroyed: usize,
    /// Emails moved off a gone label, or onto a label's id from a row
    /// keyed by its name.
    pub emails_refiled: usize,
    pub emails_upserted: usize,
    pub emails_destroyed: usize,
    /// What reading the Takeout added, when the source names one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub takeout: Option<mbox::SeedSummary>,
    /// Messages whose bytes the store held, so only their labels were
    /// asked for: from walking every label, or one `messages.get` each.
    pub relabeled_from_walks: usize,
    pub relabeled_one_by_one: usize,
    pub blobs_stored: usize,
    pub blobs_skipped: usize,
    pub blobs_oversize: usize,
    /// Messages a fetch found to carry none of the configured labels.
    pub messages_filtered: usize,
    /// Messages `messages.get` would not return, for a reason other than
    /// the message being gone. They stay owed.
    pub messages_failed: usize,
    /// Gmail quota units spent, against the per-minute ceiling.
    pub quota_units_spent: u64,
    /// True when the run stopped at `message_budget` with more to fetch.
    /// A partial backfill is a successful outcome, not a failure.
    pub budget_exhausted: bool,
    /// True when the step was asked to stop with more to fetch.
    pub interrupted: bool,
    /// What `messages.list` walked to the end this run: label names, or
    /// `*` for the whole account. Empty on a run that only replayed
    /// `history.list`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub walked: Vec<String>,
    /// Configured labels this account does not have. Reported rather
    /// than fatal: one misspelling costs that label, not the run.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<DownloadProblem>,
}

impl FetchSummary {
    /// The run ended before its work did, on purpose.
    pub fn stopped_early(&self) -> bool {
        self.budget_exhausted || self.interrupted
    }
}

fn state_scope(account_id: &str) -> String {
    format!("gmail:{account_id}:historyId")
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let (pool, stop) = (opts.db.pool().clone(), opts.control.stop.clone());
    let sealer = opts.sealer.clone();
    run_problems::collecting_sealed(&pool, &stop, sealer.as_ref(), |found| {
        sync_account(opts, found)
    })
    .await
}

async fn sync_account(opts: FetchOptions, found: RunProblems) -> Result<FetchSummary> {
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

    let result = run_sync(&db, &opts, &found).await;
    // Even on error, record a summary stub so the row has the same
    // fields a successful one does — the defaults populated as far as
    // the run got. Mirrors the JMAP path.
    let summary_for_bookkeeping = result.as_ref().cloned().unwrap_or_default();
    run.finish(&result, &summary_for_bookkeeping).await;
    result
}

async fn run_sync(db: &RawDb, opts: &FetchOptions, found: &RunProblems) -> Result<FetchSummary> {
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
    // `labels.list` is the whole set every run, so it can say which rows
    // are gone as well as which are new or renamed.
    let held = db.mailbox_names(&account_id).await?.into_keys().collect();
    let plan = index.plan_mailboxes(&account_id, &held);
    let mailbox_payloads: Vec<Value> = plan
        .rows
        .iter()
        .map(|(id, name, role)| json!({ "id": id, "name": name, "role": role }))
        .collect();
    super::upsert_mailboxes(db, &now, &account_id, &mailbox_payloads).await?;
    summary.mailboxes_upserted = mailbox_payloads.len();
    summary.emails_refiled = super::refile_mailboxes(db, &now, &plan.moves).await?;
    summary.mailboxes_destroyed = plan.moves.iter().filter(|(_, to)| to.is_none()).count();
    info!(
        event = "gmail_labels",
        account = %account_id,
        labels = mailbox_payloads.len(),
        gone = summary.mailboxes_destroyed,
        rekeyed = plan.moves.len() - summary.mailboxes_destroyed,
        "listed the account's labels",
    );

    // Turn `only_extract_labels` into Gmail label ids so a walk is
    // narrowed server-side. Doing it client-side would mean paying
    // `messages.get`'s 60 quota units for every message in the account
    // to keep a handful — see `api::list_messages`.
    let resolved = index.ids_for_names(&opts.only_labels)?;
    let filter_label_ids = resolved.resolved;
    summary.problems = resolved.problems;
    found.config(summary.problems.clone());
    if !opts.only_labels.is_empty() {
        info!(
            event = "gmail_label_filter",
            labels = %opts.only_labels.join(", "),
            ids = %filter_label_ids.join(", "),
            unresolved = summary.problems.len(),
            "restricting enumeration server-side",
        );
    }

    if let Some(path) = &opts.takeout {
        summary.takeout =
            Some(seed_from_takeout(db, opts, found, path, &account_id, &filter_label_ids).await?);
    }

    let mut run = Run {
        db,
        opts,
        found,
        index: &index,
        account_id: &account_id,
        user_id: &user_id,
        client: &client,
        messages_total: profile.messages_total,
        now: &now,
        // Nothing fixed to seed it with: the bar stays at 0/0 until the
        // owed query says how much there is to fetch.
        bar: RunBar::new(&opts.progress, 0),
    };
    let done = run
        .list_and_fetch(
            &mut throttle,
            profile.history_id.as_deref(),
            &filter_label_ids,
            &mut summary,
        )
        .await;
    run.bar.finish();
    summary.quota_units_spent = throttle.spent_total();
    // A listing or a phase the run never reached keeps its last row.
    if summary.stopped_early() {
        found.cut_short();
    }
    done?;
    info!(
        event = "gmail_summary",
        account = %account_id,
        emails_upserted = summary.emails_upserted,
        emails_destroyed = summary.emails_destroyed,
        blobs_stored = summary.blobs_stored,
        blobs_skipped = summary.blobs_skipped,
        blobs_oversize = summary.blobs_oversize,
        messages_filtered = summary.messages_filtered,
        messages_failed = summary.messages_failed,
        quota_units_spent = summary.quota_units_spent,
        walked = %summary.walked.join(", "),
        "gmail sync finished",
    );
    Ok(summary)
}

struct Run<'a> {
    db: &'a RawDb,
    opts: &'a FetchOptions,
    found: &'a RunProblems,
    index: &'a LabelIndex,
    account_id: &'a str,
    user_id: &'a str,
    client: &'a Client,
    /// `users.getProfile`'s count, which prices a walk of every label.
    messages_total: Option<u64>,
    now: &'a IsoOffsetTimestamp,
    /// The run's one progress bar. Announcing a total is what turns the
    /// Manage screen's activity cell from a bare count into "N queued"
    /// ticking down.
    bar: RunBar,
}

impl Run<'_> {
    fn stopping(&self) -> bool {
        self.opts.control.stop.requested()
    }

    async fn list_and_fetch(
        &mut self,
        throttle: &mut QuotaThrottle,
        history_id_now: Option<&str>,
        filter_label_ids: &[String],
        summary: &mut FetchSummary,
    ) -> Result<()> {
        if !self.replay_history(throttle, summary).await? {
            // `history_id_now` was read before any walk, so whatever
            // changes while one runs is replayed next run.
            listed::start_over(
                self.db.pool(),
                &state_scope(self.account_id),
                history_id_now,
                false,
            )
            .await?;
        }
        self.walk_unlisted_scopes(throttle, filter_label_ids, summary)
            .await?;
        self.fetch_owed(throttle, summary).await
    }

    /// Replay `history.list` from the stored `historyId`. Its pages are
    /// collected, then one transaction lists every message it names as
    /// added or relabeled under the new `historyId`, deletes the ones it
    /// names as deleted, and stores the new `historyId`. Whether a named
    /// message has been fetched is not asked here, so the cursor never
    /// waits on a fetch.
    ///
    /// `false` when there is nothing to replay from: no cursor,
    /// `full_resync`, or a cursor older than Google keeps history for.
    async fn replay_history(
        &self,
        throttle: &mut QuotaThrottle,
        summary: &mut FetchSummary,
    ) -> Result<bool> {
        let scope = state_scope(self.account_id);
        let stored = if self.opts.config.full_resync {
            None
        } else {
            self.db.load_scope(&scope).await?
        };
        let Some(cursor) = stored else {
            return Ok(false);
        };
        throttle.acquire(api::UNITS_HISTORY_LIST).await;
        let changes = match collect_history(self.user_id, self.client, &cursor, throttle).await {
            Ok(changes) => changes,
            // Only `history.list` reads a 404 this way.
            Err(e) if is_not_found(&e) => {
                warn!(
                    event = "gmail_history_expired",
                    account = %self.account_id,
                    cursor = %cursor,
                    "stored historyId aged out of Google's retention window; listing the account again",
                );
                return Ok(false);
            }
            Err(_) if self.stopping() => {
                summary.interrupted = true;
                return Ok(true);
            }
            Err(e) => return Err(e),
        };
        let history_id = changes.history_id.as_deref().unwrap_or(&cursor);
        info!(
            event = "gmail_history_replay",
            account = %self.account_id,
            since = %cursor,
            named = changes.named.len(),
            deleted = changes.deleted.len(),
            "replayed history since the stored cursor",
        );
        let mut tx = self.db.pool().begin().await.context("begin history tx")?;
        listed::list_in_tx(&mut tx, &changes.named, Some(history_id), Named::Changed).await?;
        summary.emails_destroyed +=
            listed::forget_in_tx(&mut tx, self.now, &changes.deleted, email_id_of).await?;
        listed::save_token_in_tx(&mut tx, &scope, history_id).await?;
        tx.commit().await.context("commit history tx")?;
        Ok(true)
    }

    /// Walk `messages.list` over every configured label (or the account)
    /// no walk has listed whole: a first run, a start over, a label the
    /// filter newly admits. Each page lists its ids; a message already
    /// listed is left as it is. A walk starts again every run until it
    /// reaches its end, and only then is its scope recorded as listed
    /// whole and, when it covered the whole account, what it did not
    /// name deleted.
    async fn walk_unlisted_scopes(
        &mut self,
        throttle: &mut QuotaThrottle,
        filter_label_ids: &[String],
        summary: &mut FetchSummary,
    ) -> Result<()> {
        let scopes = listed::scopes_owed(self.db.pool(), &admitted(filter_label_ids)).await?;
        let stamp = self.db.load_scope(&state_scope(self.account_id)).await?;
        for scope in scopes {
            if summary.stopped_early() {
                return Ok(());
            }
            let label_id = (scope != WHOLE_ACCOUNT).then_some(scope.as_str());
            let label =
                label_id.map_or_else(|| WHOLE_ACCOUNT.to_string(), |id| self.index.name(id));
            self.bar.doing(&label);
            let mut named: BTreeSet<String> = BTreeSet::new();
            let mut token: Option<String> = None;
            let mut pages = 0usize;
            let reached_the_end = loop {
                if self.stopping() {
                    summary.interrupted = true;
                    return Ok(());
                }
                throttle.acquire(api::UNITS_MESSAGES_LIST).await;
                let page = match api::list_messages(
                    self.user_id,
                    self.client,
                    token.as_deref(),
                    LIST_PAGE_SIZE,
                    label_id,
                )
                .await
                {
                    Ok(page) => page,
                    Err(_) if self.stopping() => {
                        summary.interrupted = true;
                        return Ok(());
                    }
                    // With nothing listed there is nothing to keep going
                    // for.
                    Err(e)
                        if api::is_terminal(&e) || !listed::lists_any(self.db.pool()).await? =>
                    {
                        return Err(e.context(format!("messages.list {label}")));
                    }
                    // The other walks still run, and this one is walked
                    // again by the next run, having no row to say it was
                    // listed whole.
                    Err(e) => {
                        self.found
                            .listing(&format!("messages.list {label}"), format!("{e:#}"));
                        break false;
                    }
                };
                pages += 1;
                let mut tx = self.db.pool().begin().await.context("begin listing tx")?;
                listed::list_in_tx(&mut tx, &page.ids, stamp.as_deref(), Named::Exists).await?;
                tx.commit().await.context("commit listing tx")?;
                if let Some(sealer) = &self.opts.sealer {
                    sealer.wrote(page.ids.len() as u64).await;
                }
                named.extend(page.ids);
                match page.next_page_token {
                    Some(t) => token = Some(t),
                    None => break true,
                }
            };
            if !reached_the_end {
                continue;
            }
            // A walk of the whole account also says which seeded messages
            // Gmail no longer has: they were never listed, so only their
            // rows can name them.
            summary.emails_destroyed += listed::close_enumeration(
                self.db.pool(),
                self.now,
                std::slice::from_ref(&scope),
                label_id.is_none().then_some(&named),
                label_id.is_none().then_some(self.account_id),
                email_id_of,
            )
            .await?;
            info!(
                event = "gmail_walk_done",
                label = %label,
                pages = pages,
                listed = named.len(),
                "walked one label to its end",
            );
            summary.walked.push(label);
        }
        Ok(())
    }

    /// Fetch, with `messages.get`, every listed message the store owes,
    /// newest first, until the work or the budget runs out:
    /// `owed::drain`, one `messages.get` per request, `flush_batch` to a
    /// transaction. A message Gmail no longer has, or that carries none
    /// of the configured labels, is gone: its listing, its row and
    /// whatever was held for it go. One that will not fetch or will not
    /// store stays owed.
    async fn fetch_owed(
        &mut self,
        throttle: &mut QuotaThrottle,
        summary: &mut FetchSummary,
    ) -> Result<()> {
        if summary.stopped_early() {
            return Ok(());
        }
        let pool = self.db.pool();
        let mut owed = owed::owed(pool, listed::LISTED, listed::listing(pool, true).await?).await?;
        with_the_bodiless(
            &mut owed,
            held_without_bytes(pool, self.opts.blob_size_limit_bytes).await?,
        );
        if let Some(budget) = self.opts.config.message_budget {
            if owed.len() > budget {
                summary.budget_exhausted = true;
                info!(
                    event = "gmail_budget_exhausted",
                    budget,
                    left = owed.len() - budget,
                    "fetching the budget's worth; the next run fetches what is still owed",
                );
                owed.truncate(budget);
            }
        }
        if owed.is_empty() {
            return Ok(());
        }
        let held_bytes = held_bytes(pool).await?;
        let relabeling: HashSet<String> = owed
            .iter()
            .filter(|l| held_bytes.contains_key(&email_id_of(&l.key)))
            .map(|l| l.key.clone())
            .collect();
        info!(
            event = "gmail_owed",
            account = %self.account_id,
            count = owed.len(),
            labels_only = relabeling.len(),
            "fetching the messages the store owes",
        );
        let membership = if label_walk_pays(relabeling.len(), self.index.len(), self.messages_total)
        {
            self.walk_every_label(throttle, &relabeling, summary).await
        } else {
            None
        };
        if summary.stopped_early() {
            return Ok(());
        }
        self.bar.expect(owed.len() as u64);
        self.bar.doing("fetching");

        let f = MessagesGet {
            db: self.db,
            index: self.index,
            account_id: self.account_id,
            user_id: self.user_id,
            client: self.client,
            throttle: tokio::sync::Mutex::new(throttle),
            held_bytes,
            membership,
            stop: &self.opts.control.stop,
            cap: self.opts.blob_size_limit_bytes,
            // Belt-and-braces client-side label check. A walk is narrowed
            // server-side, but `history.list` names every message in the
            // account.
            only_labels: self.opts.only_labels.iter().map(String::as_str).collect(),
            now: self.now,
            bar: &self.bar,
            filtered: AtomicUsize::new(0),
            blobs_stored: AtomicUsize::new(0),
            blobs_oversize: AtomicUsize::new(0),
            destroyed: AtomicUsize::new(0),
            relabeled_from_walks: AtomicUsize::new(0),
            relabeled_one_by_one: AtomicUsize::new(0),
        };
        let l = owed::Loop {
            pool,
            table: listed::LISTED,
            phase: P_MESSAGES_GET,
            stop: &self.opts.control.stop,
            found: self.found,
            sealer: self.opts.sealer.as_ref(),
            batch: 1,
            concurrency: 1,
            flush: self.opts.flush_batch.unwrap_or(FLUSH_BATCH),
            flush_bytes: FLUSH_BYTES,
            failures_in_a_row: 0,
        };
        let drained = owed::drain(&l, owed, &f).await?;
        summary.emails_upserted += drained.got;
        summary.messages_failed += drained.failed;
        summary.emails_destroyed += f.destroyed.load(Ordering::Relaxed);
        summary.messages_filtered += f.filtered.load(Ordering::Relaxed);
        summary.blobs_stored += f.blobs_stored.load(Ordering::Relaxed);
        summary.blobs_oversize += f.blobs_oversize.load(Ordering::Relaxed);
        summary.relabeled_from_walks += f.relabeled_from_walks.load(Ordering::Relaxed);
        summary.relabeled_one_by_one += f.relabeled_one_by_one.load(Ordering::Relaxed);
        summary.interrupted |= drained.left > 0 && self.stopping();
        // With nothing mirrored there is nothing a partial run keeps.
        if let Some(said) = drained.terminal {
            if !listed::holds_any(pool).await? {
                anyhow::bail!("messages.get: {said}");
            }
        }
        Ok(())
    }

    /// Every label of `wanted`'s messages, from one `messages.list` walk
    /// per label: a message no walk names carries none. `None` when a
    /// walk did not reach its end, since a label it missed would read as
    /// one the message lacks; the fetch then asks message by message.
    async fn walk_every_label(
        &mut self,
        throttle: &mut QuotaThrottle,
        wanted: &HashSet<String>,
        summary: &mut FetchSummary,
    ) -> Option<HashMap<String, Vec<String>>> {
        let mut membership: HashMap<String, Vec<String>> = HashMap::new();
        for label_id in self.index.ids() {
            self.bar
                .doing(&format!("labels: {}", self.index.name(label_id)));
            let mut token: Option<String> = None;
            loop {
                if self.stopping() {
                    summary.interrupted = true;
                    return None;
                }
                throttle.acquire(api::UNITS_MESSAGES_LIST).await;
                let page = match api::list_messages(
                    self.user_id,
                    self.client,
                    token.as_deref(),
                    LIST_PAGE_SIZE,
                    Some(label_id),
                )
                .await
                {
                    Ok(page) => page,
                    Err(e) => {
                        warn!(
                            event = "gmail_label_walk_failed",
                            label = %self.index.name(label_id),
                            error = %format!("{e:#}"),
                            "a label walk failed; asking for each message's labels instead",
                        );
                        return None;
                    }
                };
                for id in page.ids.into_iter().filter(|id| wanted.contains(id)) {
                    membership.entry(id).or_default().push(label_id.to_string());
                }
                match page.next_page_token {
                    Some(t) => token = Some(t),
                    None => break,
                }
            }
        }
        info!(
            event = "gmail_label_walks",
            labels = self.index.len(),
            relabeling = wanted.len(),
            "walked every label for the labels of the messages already held",
        );
        Some(membership)
    }
}

/// Whether walking every label costs less than asking each message
/// for its labels. A walk is a 5-unit page per 500 messages of each
/// label, so its pages are at most one per label plus the account's
/// size times its labels per message over 500; ten labels a message is
/// the bound taken here. One message at a time is 20 units each.
fn label_walk_pays(relabeling: usize, labels: usize, messages_total: Option<u64>) -> bool {
    const LABELS_PER_MESSAGE: u64 = 10;
    let pages = labels as u64
        + messages_total.unwrap_or(0) * LABELS_PER_MESSAGE / u64::from(LIST_PAGE_SIZE);
    let walk = pages * u64::from(api::UNITS_MESSAGES_LIST);
    relabeling as u64 * u64::from(api::UNITS_MESSAGES_GET_MINIMAL) > walk
}

/// The scopes a walk lists: `only_extract_labels` means "carrying
/// **any** of these", and `messages.list` cannot say that in one
/// request, since repeated `labelIds` intersect. So one per label, or
/// the whole account.
fn admitted(filter_label_ids: &[String]) -> BTreeSet<String> {
    if filter_label_ids.is_empty() {
        [WHOLE_ACCOUNT.to_string()].into()
    } else {
        filter_label_ids.iter().cloned().collect()
    }
}

/// Read the Takeout into the store before the API is asked: what the
/// store holds stays as it is, and once a listing has covered what the
/// source admits, a message it does not name is gone upstream and is
/// not brought back.
async fn seed_from_takeout(
    db: &RawDb,
    opts: &FetchOptions,
    found: &RunProblems,
    path: &std::path::Path,
    account_id: &str,
    filter_label_ids: &[String],
) -> Result<mbox::SeedSummary> {
    let pool = db.pool();
    let held: HashSet<String> = sqlx::query_scalar("SELECT id FROM emails")
        .fetch_all(pool)
        .await
        .context("select the emails held")?
        .into_iter()
        .collect();
    let listed = if listed::scopes_owed(pool, &admitted(filter_label_ids))
        .await?
        .is_empty()
    {
        let ids: Vec<String> = sqlx::query_scalar("SELECT id FROM listed_messages")
            .fetch_all(pool)
            .await
            .context("select the listed messages")?;
        Some(ids.iter().map(|id| email_id_of(id)).collect())
    } else {
        None
    };
    let prefix = labels::gmail_mailbox_prefix(account_id);
    let real_ids = db
        .mailbox_names(account_id)
        .await?
        .into_iter()
        .filter(|(id, _)| id.starts_with(&prefix))
        .map(|(id, name)| (name, id))
        .collect();
    mbox::seed(
        mbox::Seed {
            db,
            input_path: path.to_path_buf(),
            account_id: account_id.to_string(),
            real_ids,
            held,
            listed,
            only_labels: opts.only_labels.clone(),
            sealer: opts.sealer.as_ref(),
            progress: opts.progress.clone(),
            stop: opts.control.stop.clone(),
        },
        found,
    )
    .await
}

/// Every email whose `.eml` the store holds, with its hash and its
/// thread: what a message needs besides its labels to be written again.
async fn held_bytes(pool: &sqlx::SqlitePool) -> Result<HashMap<String, (String, String)>> {
    let rows: Vec<(String, String, String)> = sqlx::query_as(
        "SELECT e.id, b.blake3, e.thread_id FROM emails e
         JOIN email_blobs b ON b.blob_id = e.blob_id AND b.email_id = e.id
         WHERE b.blake3 IS NOT NULL",
    )
    .fetch_all(pool)
    .await
    .context("select the emails held with their bytes")?;
    Ok(rows
        .into_iter()
        .map(|(id, blake3, thread)| (id, (blake3, thread)))
        .collect())
}

/// The fetcher: one `messages.get` per request; a flush's bytes into
/// the CAS in one write, then its email rows and `.eml` edges.
struct MessagesGet<'a> {
    db: &'a RawDb,
    index: &'a LabelIndex,
    account_id: &'a str,
    user_id: &'a str,
    client: &'a Client,
    throttle: tokio::sync::Mutex<&'a mut QuotaThrottle>,
    /// Email id → the `.eml`'s hash and the thread, for every message
    /// whose bytes the store already holds: only its labels are owed.
    held_bytes: HashMap<String, (String, String)>,
    /// Gmail id → label ids, when every label was walked this run.
    membership: Option<HashMap<String, Vec<String>>>,
    stop: &'a StopFlag,
    cap: Option<u64>,
    only_labels: BTreeSet<&'a str>,
    now: &'a IsoOffsetTimestamp,
    bar: &'a RunBar,
    filtered: AtomicUsize,
    blobs_stored: AtomicUsize,
    blobs_oversize: AtomicUsize,
    destroyed: AtomicUsize,
    relabeled_from_walks: AtomicUsize,
    relabeled_one_by_one: AtomicUsize,
}

/// A fetched message: its row, its `.eml` edge, and its bytes.
struct Stored {
    row: EmailRow,
    eml: EmlBlobRow,
    body: Body,
}

/// What a fetched message's `.eml` comes to.
enum Body {
    /// Bytes for the CAS.
    New(Vec<u8>),
    /// The store holds them and their edge already.
    Held,
    /// Left out, and why: over the cap.
    OverCap(String),
}

impl MessagesGet<'_> {
    /// What a fetched message comes to: gone when it carries none of
    /// the configured labels; else its row, with its bytes when they fit
    /// under the cap and a warning on its `.eml` when not.
    fn keep(&self, ingested: Ingested, held: bool) -> Outcome<Stored> {
        if !self.only_labels.is_empty()
            && !ingested
                .label_paths
                .iter()
                .any(|p| self.only_labels.contains(p.as_str()))
        {
            self.filtered.fetch_add(1, Ordering::Relaxed);
            return Outcome::Gone;
        }
        let eml = EmlBlobRow::new(&ingested.email_id, &ingested.blob_id);
        let body = match self.cap.filter(|cap| ingested.raw.len() as u64 > *cap) {
            _ if held => Body::Held,
            Some(cap) => {
                self.blobs_oversize.fetch_add(1, Ordering::Relaxed);
                Body::OverCap(format!(
                    "the .eml is {} bytes, over blob_size_limit_bytes ({cap})",
                    ingested.raw.len()
                ))
            }
            None => {
                self.blobs_stored.fetch_add(1, Ordering::Relaxed);
                Body::New(ingested.raw)
            }
        };
        Outcome::Got(Stored {
            row: ingested.row,
            eml,
            body,
        })
    }

    /// A message whose bytes the store holds: its labels from the walks
    /// when every label was walked, else from a `messages.get` that
    /// leaves the message out. `None` when the CAS no longer has the
    /// bytes, so the message is fetched whole.
    async fn relabel(&self, id: &str, blake3: &str, thread: &str) -> Result<Option<GmailMessage>> {
        let Some(object) = self.db.cas().get(blake3).await? else {
            return Ok(None);
        };
        let mut msg = match &self.membership {
            Some(membership) => {
                self.relabeled_from_walks.fetch_add(1, Ordering::Relaxed);
                GmailMessage {
                    id: id.to_string(),
                    thread_id: ingest::gmail_thread_id(thread).unwrap_or_else(|| id.to_string()),
                    label_ids: membership.get(id).cloned().unwrap_or_default(),
                    history_id: None,
                    internal_date_ms: None,
                    size_estimate: None,
                    raw: Vec::new(),
                }
            }
            None => {
                self.throttle
                    .lock()
                    .await
                    .acquire(api::UNITS_MESSAGES_GET_MINIMAL)
                    .await;
                self.relabeled_one_by_one.fetch_add(1, Ordering::Relaxed);
                api::get_message_minimal(self.user_id, self.client, id).await?
            }
        };
        msg.raw = object.bytes;
        Ok(Some(msg))
    }

    async fn get(&self, id: &str) -> Result<(GmailMessage, bool)> {
        if let Some((blake3, thread)) = self.held_bytes.get(&email_id_of(id)) {
            if let Some(msg) = self.relabel(id, blake3, thread).await? {
                return Ok((msg, true));
            }
        }
        self.throttle
            .lock()
            .await
            .acquire(api::UNITS_MESSAGES_GET_RAW)
            .await;
        Ok((
            api::get_message_raw(self.user_id, self.client, id).await?,
            false,
        ))
    }
}

#[async_trait]
impl Fetcher<Stored> for MessagesGet<'_> {
    async fn fetch(
        &self,
        batch: Vec<Listed>,
    ) -> std::result::Result<Vec<Fetched<Stored>>, BatchError> {
        let mut answers = Vec::with_capacity(batch.len());
        for listed in batch {
            let got = self.get(&listed.key).await;
            self.bar.did(1);
            let outcome = match got {
                Ok((msg, held)) => match ingest::ingest(self.account_id, self.index, &msg) {
                    Ok(ingested) => self.keep(ingested, held),
                    Err(e) => Outcome::Failed(format!("{e}")),
                },
                // Deleted since it was listed: normal on a busy mailbox,
                // and nothing to come back for.
                Err(e) if is_not_found(&e) => Outcome::Gone,
                // A cancel that lands mid-backoff arrives here as an
                // error, and it is not one.
                Err(e) if self.stop.requested() => return Err(BatchError::Batch(e)),
                Err(e) if api::is_terminal(&e) => return Err(BatchError::Terminal(e)),
                Err(e) => Outcome::Failed(format!("{e}")),
            };
            answers.push(Fetched { listed, outcome });
        }
        Ok(answers)
    }

    /// The flush's bytes into the CAS; then the email rows, their joins
    /// and threads, and the messages that are gone
    /// (`listed::write_batch_in_tx`); then the `.eml` edges, whose
    /// sidecar is this fetcher's to stamp, the listing's being the loop's.
    async fn store(
        &self,
        tx: &mut Transaction<'static, Sqlite>,
        batch: &[Fetched<Stored>],
    ) -> Result<()> {
        let mut got = Vec::new();
        let mut gone = Vec::new();
        let mut edges = Vec::new();
        for f in batch {
            match &f.outcome {
                Outcome::Got(s) | Outcome::Unusable(s, ..) => {
                    got.push(s.row.clone());
                    edges.push(s);
                }
                Outcome::Gone => gone.push(f.listed.key.clone()),
                Outcome::Failed(_) | Outcome::Skipped(..) => {}
            }
        }
        edges.retain(|s| !matches!(s.body, Body::Held));
        let inserts: Vec<CasInsert<'_, &str>> = edges
            .iter()
            .filter_map(|s| match &s.body {
                Body::New(bytes) => Some(CasInsert {
                    id: s.eml.id.as_str(),
                    bytes,
                    content_type: Some("message/rfc822"),
                }),
                Body::Held | Body::OverCap(_) => None,
            })
            .collect();
        let stored = self.db.cas().put_many(inserts).await?;
        let n = listed::write_batch_in_tx(tx, self.now, got, &gone, email_id_of).await?;
        self.destroyed.fetch_add(n, Ordering::Relaxed);

        let rows: Vec<EmlBlobRow> = edges
            .iter()
            .map(|s| EmlBlobRow {
                blake3: stored.get(s.eml.id.as_str()).cloned(),
                ..s.eml.clone()
            })
            .collect();
        write_eml_edges_in_tx(tx, &rows).await?;
        let landed = rows
            .iter()
            .filter(|r| r.blake3.is_some())
            .map(|r| r.id.as_str());
        bulk_upsert_bookkeeping(tx, EmlBlobRow::TABLE, landed, self.now).await?;
        for s in &edges {
            if let Body::OverCap(why) = &s.body {
                dr::record_object_skipped(
                    tx,
                    EmlBlobRow::TABLE,
                    &s.eml.id,
                    datalib_problems::Reason::OverSizeLimit,
                    why,
                )
                .await?;
            }
        }
        Ok(())
    }

    fn weight(&self, s: &Stored) -> usize {
        match &s.body {
            Body::New(bytes) => bytes.len(),
            Body::Held | Body::OverCap(_) => 0,
        }
    }
}

/// How the `emails` row for a Gmail message is keyed: `GmailId`'s key,
/// the one `ingest::ingest` mints.
fn email_id_of(gmail_id: &str) -> String {
    GmailId::from_api(gmail_id).map_or_else(|| gmail_id.to_string(), GmailId::key)
}

/// [`email_id_of`] in SQL: Gmail's hex id, zero-padded to sixteen.
const EMAIL_ID_OF_SQL: &str = "substr('0000000000000000' || lower(l.id), -16)";

/// Also owed: a listed message whose email has no `.eml` stored and fits
/// under `cap`. Its listing says nothing about its bytes, so the loop
/// alone would not ask for it again once the cap allows it.
async fn held_without_bytes(pool: &sqlx::SqlitePool, cap: Option<u64>) -> Result<Vec<Listed>> {
    let rows: Vec<(String, Option<String>)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        // Audited: a constant expression over the row; the cap is bound.
        "SELECT l.id, l.stamp FROM listed_messages l
         JOIN emails e ON e.id = {EMAIL_ID_OF_SQL}
         WHERE (?1 IS NULL OR e.size <= ?1)
           AND NOT EXISTS (SELECT 1 FROM email_blobs b
                           WHERE b.blob_id = e.blob_id AND b.blake3 IS NOT NULL)
         ORDER BY l.id DESC"
    )))
    .bind(cap.map(|c| c as i64))
    .fetch_all(pool)
    .await
    .context("select the messages held without their bytes")?;
    Ok(rows
        .into_iter()
        .map(|(id, stamp)| Listed::new(id, stamp))
        .collect())
}

/// `owed` with the messages of `bodiless` it does not already name,
/// newest first.
fn with_the_bodiless(owed: &mut Vec<Listed>, bodiless: Vec<Listed>) {
    let named: BTreeSet<String> = owed.iter().map(|l| l.key.clone()).collect();
    owed.extend(bodiless.into_iter().filter(|l| !named.contains(&l.key)));
    owed.sort_by(|a, b| b.key.cmp(&a.key));
}

#[derive(Debug, Default)]
struct Changes {
    /// Added or relabeled: either way the message is to be fetched.
    named: Vec<String>,
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
        out.named.extend(page.added);
        out.named.extend(page.relabeled);
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
    out.deleted.sort();
    out.deleted.dedup();
    out.named.sort();
    out.named.dedup();
    out.named.retain(|id| !out.deleted.contains(id));
    Ok(out)
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

    /// A 404 has to be recognized through anyhow's context chain — it
    /// arrives wrapped. Both callers depend on this: `history.list`
    /// reads it as an expired cursor, `messages.get` as a deleted
    /// message, and everything else is a failure that leaves the message
    /// owed.
    /// The SQL spelling of the email id and `ingest::ingest`'s agree,
    /// or a message held without its bytes is never asked for again.
    #[tokio::test]
    async fn the_sql_email_id_is_the_one_ingest_mints() {
        let d = tempfile::tempdir().unwrap();
        let pool = datalib_etl::doltlite_raw::open(&d.path().join("k.doltlite_db"), &[])
            .await
            .unwrap();
        for id in ["18c9f2a1b2c3d601", "18C9F2A1B2C3D601", "ab", "0"] {
            let sql = format!("SELECT {}", EMAIL_ID_OF_SQL.replace("l.id", "?"));
            // Audited: a constant expression; the id is bound.
            let in_sql: String = sqlx::query_scalar(sqlx::AssertSqlSafe(sql))
                .bind(id)
                .fetch_one(&pool)
                .await
                .unwrap();
            assert_eq!(in_sql, email_id_of(id), "{id}");
        }
        pool.close().await;
    }

    #[test]
    fn recognizes_a_404_through_context() {
        let e = anyhow::Error::new(api::GmailApiError::NotFound)
            .context("users.history.list")
            .context("while syncing");
        assert!(is_not_found(&e));
        assert!(!is_not_found(&anyhow::anyhow!("some other failure")));
    }
}
