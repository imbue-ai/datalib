//! Gmail REST API downloader — the third mode of `type: email`.
//!
//! `history.list` or a `messages.list` walk says which messages exist
//! and which changed; `super::listed` keeps that apart from what has
//! been fetched, and `messages.get` is asked for whatever the store owes.

pub mod api;
pub mod ingest;

use std::collections::BTreeSet;

use anyhow::{Context, Result};
use datalib_etl::blob_cas::{CasEdgeAccumulator, CasEdgeRow as _};
use datalib_etl::control::DownloadControl;
use datalib_etl::download_problems::DownloadProblem;
use datalib_etl::download_run::DownloadRun;
use datalib_etl::http::LatchkeySettings;
use datalib_etl::progress::{Progress, RunBar};
use datalib_etl::run_problems::{self, RunProblems};
use datalib_time::IsoOffsetTimestamp;
use serde::Serialize;
use serde_json::{json, Value};
use tracing::{info, warn};

use datalib_etl_email_config::EmailGmailApi;

use super::db::RawDb;
use super::listed::{self, Held, Named, WHOLE_ACCOUNT};
use super::schema_raw::EmlBlobRow;
use api::{Client, QuotaThrottle};
use ingest::LabelIndex;

/// `messages.list` page size. Google's maximum is 500; ids are tiny, so
/// there is no reason to ask for less.
const LIST_PAGE_SIZE: u32 = 500;
/// Fetched messages held in memory before they are written.
const FLUSH_BATCH: usize = 200;

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
    /// When non-empty, only ingest messages carrying at least one label
    /// whose canonical path exactly matches one of these.
    pub only_labels: Vec<String>,
    pub blob_size_limit_bytes: Option<u64>,
    /// Messages fetched before they are written. `None` is [`FLUSH_BATCH`].
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
    // `messages.get`'s 20 quota units for every message in the account
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

    let mut run = Run {
        db,
        opts,
        found,
        index: &index,
        account_id: &account_id,
        user_id: &user_id,
        client: &client,
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
            listed::forget_in_tx(&mut tx, self.now, &changes.deleted).await?;
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
        // `only_extract_labels` means "carrying **any** of these", and
        // `messages.list` cannot say that in one request: repeated
        // `labelIds` intersect. So one walk per label.
        let admitted: BTreeSet<String> = if filter_label_ids.is_empty() {
            [WHOLE_ACCOUNT.to_string()].into()
        } else {
            filter_label_ids.iter().cloned().collect()
        };
        let scopes = listed::scopes_owed(self.db.pool(), &admitted).await?;
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
            summary.emails_destroyed += listed::close_enumeration(
                self.db.pool(),
                self.now,
                std::slice::from_ref(&scope),
                label_id.is_none().then_some(&named),
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

    /// Fetch, with `messages.get`, every listed message the store owes
    /// (`listed::owed_with_bodies`), newest first, until the work, the
    /// budget or the quota runs out. A message Gmail no longer has, or
    /// that carries none of the configured labels, loses its listing and
    /// whatever was held for it. One that will not fetch stays owed.
    async fn fetch_owed(
        &mut self,
        throttle: &mut QuotaThrottle,
        summary: &mut FetchSummary,
    ) -> Result<()> {
        if summary.stopped_early() {
            return Ok(());
        }
        let build = build_identity();
        let owed =
            listed::owed_with_bodies(self.db.pool(), &build, self.opts.blob_size_limit_bytes)
                .await?;
        if owed.is_empty() {
            return Ok(());
        }
        info!(
            event = "gmail_owed",
            account = %self.account_id,
            count = owed.len(),
            "fetching the messages the store owes",
        );
        self.bar.expect(owed.len() as u64);
        self.bar.doing("fetching");
        // Belt-and-braces client-side label check. A walk is narrowed
        // server-side, but `history.list` names every message in the
        // account.
        let only_labels: BTreeSet<&str> =
            self.opts.only_labels.iter().map(String::as_str).collect();
        let flush_at = self.opts.flush_batch.unwrap_or(FLUSH_BATCH);
        let mut pending = Pending {
            known_blobs: self.db.loaded_blob_ids().await?.into_keys().collect(),
            ..Default::default()
        };
        let mut fetched = 0usize;
        // What ends the fetch early: a refused credential, a spent daily
        // quota, a retry loop that gave up. Walking on would fail every
        // remaining message the same way, one attempt each.
        let mut gave_up: Option<anyhow::Error> = None;
        for message in &owed {
            if self.stopping() {
                summary.interrupted = true;
                break;
            }
            if self
                .opts
                .config
                .message_budget
                .is_some_and(|b| fetched >= b)
            {
                summary.budget_exhausted = true;
                info!(
                    event = "gmail_budget_exhausted",
                    fetched,
                    left = owed.len() - fetched,
                    "stopping early with a partial result; the next run fetches what is still owed",
                );
                break;
            }
            throttle.acquire(api::UNITS_MESSAGES_GET).await;
            let msg = match api::get_message_raw(self.user_id, self.client, &message.id).await {
                Ok(m) => m,
                Err(e) if is_not_found(&e) => {
                    // Deleted since it was listed: normal on a busy
                    // mailbox, and nothing to come back for.
                    summary.emails_destroyed += self.forget(&message.id).await?;
                    self.bar.did(1);
                    continue;
                }
                // A cancel that lands mid-backoff arrives here as an
                // error, and it is not one.
                Err(_) if self.stopping() => {
                    summary.interrupted = true;
                    break;
                }
                Err(e) if api::is_terminal(&e) => {
                    gave_up = Some(e);
                    break;
                }
                Err(e) => {
                    summary.messages_failed += 1;
                    let ids = std::slice::from_ref(&message.id);
                    listed::record_failures(self.db.pool(), ids, &format!("{e}")).await?;
                    self.bar.did(1);
                    continue;
                }
            };
            fetched += 1;
            self.bar.did(1);

            let ingested = match ingest::ingest(self.account_id, self.index, &msg) {
                Ok(i) => i,
                Err(e) => {
                    listed::mark_unstorable(self.db.pool(), message, &build, &format!("{e}"))
                        .await?;
                    continue;
                }
            };
            if !only_labels.is_empty()
                && !ingested
                    .label_paths
                    .iter()
                    .any(|p| only_labels.contains(p.as_str()))
            {
                summary.messages_filtered += 1;
                summary.emails_destroyed += self.forget(&message.id).await?;
                continue;
            }

            let oversize = self
                .opts
                .blob_size_limit_bytes
                .filter(|cap| ingested.raw.len() as u64 > *cap);
            if let Some(cap) = oversize {
                summary.blobs_oversize += 1;
                pending.cas.add_skipped(
                    &ingested.email_id,
                    &ingested.blob_id,
                    datalib_problems::Reason::OverSizeLimit,
                    format!(
                        "the .eml is {} bytes, over blob_size_limit_bytes ({cap})",
                        ingested.raw.len()
                    ),
                );
            } else if !pending.known_blobs.insert(ingested.blob_id.clone()) {
                summary.blobs_skipped += 1;
            } else {
                pending.cas.add_fetched(
                    &ingested.email_id,
                    &ingested.blob_id,
                    ingested.raw,
                    Some("message/rfc822".to_string()),
                    None,
                );
                summary.blobs_stored += 1;
            }
            pending.held.push(Held {
                id: message.id.clone(),
                stamp: message.stamp.clone(),
                row: ingested.row,
            });
            if pending.held.len() >= flush_at {
                self.flush(&mut pending, summary).await?;
            }
        }
        self.flush(&mut pending, summary).await?;

        let Some(e) = gave_up else {
            return Ok(());
        };
        // With nothing mirrored there is nothing a partial run keeps.
        if !listed::holds_any(self.db.pool()).await? {
            return Err(e.context("messages.get"));
        }
        let left = owed.len() - fetched;
        warn!(
            event = "gmail_fetch_stopped",
            fetched,
            left,
            error = %format!("{e:#}"),
            "the fetch gave up; the next run fetches what is still owed"
        );
        self.found.phase(
            P_MESSAGES_GET,
            format!("{fetched} fetched, {left} left: {e:#}"),
        );
        self.found.cut_short();
        Ok(())
    }

    async fn forget(&self, gmail_id: &str) -> Result<usize> {
        let mut tx = self.db.pool().begin().await.context("begin forget tx")?;
        let ids = [gmail_id.to_string()];
        let gone = listed::forget_in_tx(&mut tx, self.now, &ids).await?;
        tx.commit().await.context("commit forget tx")?;
        Ok(gone)
    }

    /// One transaction writes the fetched messages: email rows, thread
    /// rows and the stamp each fetch satisfies. Their `.eml` bytes follow
    /// through the shared CAS-edge write; a message whose bytes never
    /// land is owed again by having none.
    async fn flush(&self, pending: &mut Pending, summary: &mut FetchSummary) -> Result<()> {
        let held = std::mem::take(&mut pending.held);
        if held.is_empty() {
            return Ok(());
        }
        let written = held.len();
        summary.emails_upserted += written;
        let mut tx = self.db.pool().begin().await.context("begin messages tx")?;
        listed::hold_in_tx(&mut tx, self.now, held).await?;
        tx.commit().await.context("commit messages tx")?;

        std::mem::take(&mut pending.cas)
            .flush(
                self.db.pool(),
                self.db.cas(),
                |email_id, blob_id, blake3| EmlBlobRow {
                    id: EmlBlobRow::pk_recipe(email_id, blob_id),
                    email_id: email_id.to_string(),
                    blob_id: blob_id.to_string(),
                    blake3: blake3.map(str::to_string),
                },
            )
            .await?;
        if let Some(sealer) = &self.opts.sealer {
            sealer.wrote(written as u64).await;
        }
        Ok(())
    }
}

/// Fetched messages waiting for one write.
#[derive(Default)]
struct Pending {
    held: Vec<Held>,
    cas: CasEdgeAccumulator,
    /// `.eml` blobs stored already, or waiting in `cas`.
    known_blobs: BTreeSet<String>,
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

/// The build that is running: a message it could not store is tried
/// again only by another.
fn build_identity() -> String {
    format!(
        "{}+{}",
        datalib_runtime::build_id::DATALIB_VERSION,
        datalib_runtime::build_id::git_hash().unwrap_or_default()
    )
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
    #[test]
    fn recognizes_a_404_through_context() {
        let e = anyhow::Error::new(api::GmailApiError::NotFound)
            .context("users.history.list")
            .context("while syncing");
        assert!(is_not_found(&e));
        assert!(!is_not_found(&anyhow::anyhow!("some other failure")));
    }
}
