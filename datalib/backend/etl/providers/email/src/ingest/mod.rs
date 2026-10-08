//! JMAP downloader: mailboxes, then what `Email/changes` or an
//! `Email/query` enumeration lists, then the emails the store owes
//! (`listed`), then the `.eml` of every email that has none.

pub mod api;
pub mod db;
pub mod envelope;
pub mod gmail_api;
pub mod labels;
pub mod listed;
pub mod mbox;
pub mod schema_raw;
pub mod session;

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use datalib_etl::blob_cas::{blake3_hex, CasEdgeRow as _, CasInsert};
use datalib_etl::bulk::{bulk_upsert_in_tx, BulkUpsertable as _};
use datalib_etl::download_run::DownloadRun;
use datalib_etl::progress::{Progress, RunBar};
use datalib_etl::run_problems::{self, RunProblems};
use datalib_etl_web::http::LatchkeySettings;
use datalib_etl_web::owed::{self, BatchError, Fetched, Fetcher, Listed, Outcome};
use datalib_time::IsoOffsetTimestamp;
use serde::Serialize;
use serde_json::{json, Value};
use sqlx::{Sqlite, Transaction};
use tracing::{debug, info, warn};

pub use db::{block_on_load_all, db_path_for, LoadedRaw, RawDb};

use api::call;
use datalib_etl::doltlite_raw as dr;
use db::{refresh_email_joins, write_eml_edges_in_tx};
use listed::{Named, WHOLE_ACCOUNT};
use schema_raw::{AccountRow, EmailRow, EmlBlobRow, MailboxRow, MAILBOX_VOLATILE_PATHS};

async fn upsert_account(
    db: &RawDb,
    now: &IsoOffsetTimestamp,
    id: &str,
    payload: &Value,
) -> Result<()> {
    let row = AccountRow::from_jmap_payload(id, payload)?;
    let mut tx = db.pool().begin().await.context("begin account tx")?;
    bulk_upsert_in_tx(&mut tx, std::slice::from_ref(&row), now).await?;
    tx.commit().await.context("commit account tx")?;
    Ok(())
}

async fn upsert_mailboxes(
    db: &RawDb,
    now: &IsoOffsetTimestamp,
    account_id: &str,
    payloads: &[Value],
) -> Result<()> {
    if payloads.is_empty() {
        return Ok(());
    }
    let mut rows: Vec<MailboxRow> = Vec::with_capacity(payloads.len());
    let mut volatile: Vec<(String, Value)> = Vec::new();
    for p in payloads {
        let (content, counts) = dr::split_volatile(p, MAILBOX_VOLATILE_PATHS);
        let row = MailboxRow::from_jmap_payload(account_id, &content)?;
        if let Some(counts) = counts {
            volatile.push((row.id_and_payload.id.clone(), counts));
        }
        rows.push(row);
    }
    let volatile: Vec<(&str, &Value)> = volatile.iter().map(|(id, v)| (id.as_str(), v)).collect();
    let mut tx = db.pool().begin().await.context("begin mailboxes tx")?;
    bulk_upsert_in_tx(&mut tx, &rows, now).await?;
    dr::set_volatile_payloads_in_tx(&mut tx, "mailboxes", &volatile).await?;
    tx.commit().await.context("commit mailboxes tx")?;
    Ok(())
}

/// Move every email filed under each `from` mailbox to its `to`, or off
/// it when `to` is `None`, then drop the `from` row. Payload and join rows
/// move together, so the `emails` diff and the `email_mailboxes` diff tell
/// the same story. Returns how many emails moved.
///
/// For a label that went away upstream (`None`), and for a row whose id
/// changed recipe while the label stayed (`Some`).
pub(crate) async fn refile_mailboxes(
    db: &RawDb,
    now: &IsoOffsetTimestamp,
    moves: &[(String, Option<String>)],
) -> Result<usize> {
    let mut moved = 0;
    for (from, to) in moves {
        let mut after = String::new();
        loop {
            let batch = db.emails_filed_under(from, &after, REFILE_BATCH).await?;
            let Some((last, _, _)) = batch.last() else {
                break;
            };
            after = last.clone();
            let rows: Vec<EmailRow> = batch
                .into_iter()
                .filter_map(|(_, account, payload)| {
                    EmailRow::from_jmap_envelope(&account, &refiled(payload, from, to.as_deref()))
                })
                .collect();
            upsert_emails(db, now, &rows).await?;
            moved += rows.len();
        }
    }
    let gone: Vec<String> = moves.iter().map(|(from, _)| from.clone()).collect();
    db.delete_mailboxes(&gone).await?;
    if !gone.is_empty() {
        info!(
            event = "email_mailboxes_refiled",
            mailboxes = gone.len(),
            emails = moved,
            "moved emails off mailboxes that are gone or re-keyed",
        );
    }
    Ok(moved)
}

/// Emails per transaction when [`refile_mailboxes`] rewrites them.
const REFILE_BATCH: usize = 500;

fn refiled(mut payload: Value, from: &str, to: Option<&str>) -> Value {
    if let Some(ids) = payload.get_mut("mailboxIds").and_then(Value::as_object_mut) {
        ids.remove(from);
        if let Some(to) = to {
            ids.insert(to.to_string(), Value::Bool(true));
        }
    }
    payload
}

/// The mailbox rows a complete listing of an account's mailboxes leaves
/// behind: every row of `held` it did not name.
fn unlisted_mailboxes<'a>(
    held: impl IntoIterator<Item = &'a String>,
    listed: &HashSet<String>,
) -> Vec<(String, Option<String>)> {
    held.into_iter()
        .filter(|id| !listed.contains(*id))
        .map(|id| (id.clone(), None))
        .collect()
}

async fn upsert_emails(db: &RawDb, now: &IsoOffsetTimestamp, rows: &[EmailRow]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut tx = db.pool().begin().await.context("begin emails tx")?;
    bulk_upsert_in_tx(&mut tx, rows, now).await?;
    for row in rows {
        refresh_email_joins(&mut tx, row).await?;
    }
    tx.commit().await.context("commit emails tx")?;
    Ok(())
}
use session::Session;

/// Batch size for `Email/get` detail fetches. JMAP servers typically
/// cap a single `Email/get` at ~500 ids; we stay well below to keep
/// per-call latency bounded.
const EMAIL_GET_BATCH: usize = 50;
/// Ids asked of `Email/query` per page of an enumeration.
const EMAIL_QUERY_PAGE: usize = 500;
/// `Email/changes` `maxChanges` ceiling — large enough to drain a
/// month of activity in one call, small enough that a single response
/// stays under a megabyte.
const CHANGES_MAX: u64 = 5_000;
/// Per-request timeout for blob downloads. Big attachments take time.
const BLOB_TIMEOUT: Duration = Duration::from_secs(180);
/// Default number of `.eml` downloads to keep in flight in the blob
/// phase when the config leaves `blob_download_concurrency` unset. JMAP
/// has no bulk-download method, so concurrency is the only lever for a
/// large initial backfill; this value is a polite-but-useful fan-out
/// against the download endpoint. Override per-source in the `sync:`
/// block; set `1` to restore strictly-serial fetching.
const DEFAULT_BLOB_CONCURRENCY: usize = 8;

/// Envelope-only `Email/get` properties. Body parts (`bodyValues`,
/// `textBody`, `htmlBody`, `preview`) are deliberately omitted: the
/// canonical body source is the `.eml` blob in the shared CAS, and
/// render `mail-parse`s it on demand so the JMAP and mbox sources
/// feed identical inputs into the renderer.
const EMAIL_GET_PROPERTIES: &[&str] = &[
    "id",
    "blobId",
    "threadId",
    "mailboxIds",
    "keywords",
    "from",
    "subject",
    "sentAt",
    "receivedAt",
    "size",
    "messageId",
    "hasAttachment",
    "attachments",
];

#[derive(Debug, Clone)]
pub struct FetchOptions {
    /// Which latchkey identity the download authenticates as, from the
    /// source's `latchkey_settings:` block.
    pub latchkey: LatchkeySettings,
    /// The store this run writes into, opened and closed by the caller.
    /// A download never opens a store of its own: one writer per file
    /// (`datalib/backend/etl/README.md` § "One writer per file, by
    /// construction").
    pub db: RawDb,
    /// Seals a flushed batch, so render can start on the mail already
    /// mirrored while the walk continues. `None` commits once at the end.
    pub sealer: Option<datalib_etl::raw_store::Sealer>,
    pub hostname: String,
    pub account_id: Option<String>,
    /// Skip the stored state tokens: list every mailbox and every email
    /// again, and fetch every email again.
    pub full_resync: bool,
    /// When non-empty, restrict the sync to mailboxes whose full label
    /// path (POSIX-like, e.g. `Work/Projects`; see
    /// [`crate::mailbox_labels`]) exactly matches one of these. Empty =
    /// every mailbox the account exposes. The paths are resolved to
    /// JMAP mailbox ids once `Mailbox/get` has run. An enumeration is
    /// narrowed to them server-side; `Email/changes` is account-wide, so
    /// an email it names is checked against them once fetched.
    pub only_mailbox_labels: Vec<String>,
    /// Skip downloading any blob whose advertised size exceeds this.
    /// `None` = no limit.
    pub blob_size_limit_bytes: Option<u64>,
    /// How many `.eml` downloads to keep in flight at once during the
    /// blob phase. `None` → [`DEFAULT_BLOB_CONCURRENCY`]; clamped to ≥ 1.
    pub blob_download_concurrency: Option<usize>,
    /// How many `.eml` outcomes the blob phase writes in one
    /// transaction. `None` → [`BLOB_FLUSH_COUNT`].
    pub blob_flush_count: Option<usize>,
    /// How many bytes of `.eml` bodies the blob phase holds before it
    /// writes them, whatever the count. `None` → [`BLOB_FLUSH_BYTES`].
    pub blob_flush_bytes: Option<usize>,
    pub progress: Progress,
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
            hostname: String::new(),
            account_id: None,
            full_resync: false,
            only_mailbox_labels: Vec::new(),
            blob_size_limit_bytes: None,
            blob_download_concurrency: None,
            blob_flush_count: None,
            blob_flush_bytes: None,
            progress: Progress::noop(),
            control: datalib_etl::control::DownloadControl::default(),
        }
    }
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct FetchSummary {
    /// Configured mailbox paths this account does not have. Reported
    /// rather than fatal, the same as every other provider's.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<datalib_etl::download_problems::DownloadProblem>,
    pub account_id: String,
    pub mailboxes_upserted: usize,
    pub mailboxes_destroyed: usize,
    pub emails_upserted: usize,
    pub emails_destroyed: usize,
    pub blobs_downloaded: usize,
    pub blobs_skipped: usize,
    pub blobs_errored: usize,
    pub blobs_oversize: usize,
}

/// The config key a [`datalib_etl::download_problems::DownloadProblem`]
/// about the label filter names.
pub(crate) const K_ONLY_EXTRACT_LABELS: &str = "only_extract_labels";

/// Session, mailboxes, emails, blobs: the four ticks the outer bar
/// makes whatever the run turns out to hold.
const PHASES: u64 = 4;

/// The listing and phase names a `problems` row carries.
const M_MAILBOX_GET: &str = "Mailbox/get";
const M_EMAIL_CHANGES: &str = "Email/changes";
const M_EMAIL_QUERY: &str = "Email/query";
const M_EMAIL_GET: &str = "Email/get";
const M_EML_DOWNLOAD: &str = "eml_download";

/// `Email/get` batches in a row that may fail before the phase stops.
const GET_FAILURE_BUDGET: usize = 3;

/// `.eml` downloads in a row that may fail before the phase stops: past
/// this many, something is wrong with every download, not with one.
const BLOB_FAILURE_BUDGET: usize = 20;
/// `.eml` outcomes the blob phase writes in one transaction, and the
/// bytes of bodies that force a write sooner: the bodies wait in memory
/// until their flush.
const BLOB_FLUSH_COUNT: usize = 256;
const BLOB_FLUSH_BYTES: usize = 32 * 1024 * 1024;

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

    // Coarse per-phase progress so the bar moves even though we don't
    // have a meaningful per-item denominator before the first JMAP
    // response. Without this, fastmail looks stuck at 0/0 in the
    // dashboard whether it's running or wedged on Session::discover.
    // Each phase below adds its own real total to this as it learns it.
    let bar = RunBar::new(&opts.progress, PHASES);
    bar.doing("session");

    let session = Session::discover(&opts.hostname, &opts.latchkey)
        .await
        .with_context(|| format!("discover JMAP session at {}", opts.hostname))?;
    bar.did(1);
    let account_id = session.pick_account(opts.account_id.as_deref())?;
    info!(
        event = "jmap_session",
        hostname = %opts.hostname,
        account_id = %account_id,
        api_url = %session.api_url,
        "opened the JMAP session"
    );

    // Stamp the run + a record of the account itself.
    let run = DownloadRun::start(
        db.pool(),
        &json!({
            "hostname": opts.hostname,
            "account_id": account_id,
            "full_resync": opts.full_resync,
            "only_mailbox_labels": opts.only_mailbox_labels,
        }),
    )
    .await?;

    let result = run_sync(&db, &session, &account_id, &opts, &bar, &found).await;
    bar.finish();
    // On error we still serialize a partial-summary stub so the row
    // has fields for grafana-style dashboards to graph. The summary
    // type is the same on both paths; on error its fields will simply
    // be the defaults populated up to the failure point.
    let summary_for_bookkeeping = result.as_ref().cloned().unwrap_or_default();
    run.finish(&result, &summary_for_bookkeeping).await;
    result
}

async fn run_sync(
    db: &RawDb,
    session: &Session,
    account_id: &str,
    opts: &FetchOptions,
    bar: &RunBar,
    found: &RunProblems,
) -> Result<FetchSummary> {
    let mut summary = FetchSummary {
        account_id: account_id.to_string(),
        ..Default::default()
    };

    // One timestamp per fetch run, threaded into every
    // `bulk_upsert_in_tx` call below. Goes into the bookkeeping
    // sidecars' `fetched_at_utc` / `last_attempt_at_utc` columns; the value
    // means "the sync that wrote this row," not "the millisecond the
    // UPSERT query ran" — so consistency across tables matters more
    // than sub-second freshness.
    let now = IsoOffsetTimestamp::now_local();

    // Persist the account row.
    let account_payload = session
        .accounts
        .iter()
        .find(|(k, _)| k == account_id)
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| json!({}));
    upsert_account(db, &now, account_id, &account_payload).await?;

    // ── mailboxes ───────────────────────────────────────────────────
    bar.doing("mailboxes");
    let listed = sync_mailboxes(db, &now, session, account_id, opts, &mut summary).await;
    // The mailboxes an earlier listing stored still file the mail; with
    // none stored there is nothing to file it under.
    let stored = !db.mailbox_names(account_id).await?.is_empty();
    or_a_listing_problem(listed, M_MAILBOX_GET, stored, opts, found)?;
    bar.did(1);

    // Resolve the configured label paths to mailbox ids now that the
    // full tree is in the db. Empty config = no filter (sync every
    // mailbox). An all-unmatched filter resolves to an empty set, which
    // means "match nothing" — reported below so a typo'd path doesn't
    // silently drop the whole account.
    let mailbox_filter: Option<BTreeSet<String>> = if opts.only_mailbox_labels.is_empty() {
        None
    } else {
        let nodes: Vec<crate::mailbox_labels::MailboxNode> = db
            .load_mailboxes()
            .await?
            .iter()
            .filter_map(crate::mailbox_labels::MailboxNode::from_payload)
            .collect();
        let resolved = crate::mailbox_labels::resolve(&nodes, &opts.only_mailbox_labels);
        for spec in &resolved.unmatched {
            summary
                .problems
                .push(datalib_etl::download_problems::DownloadProblem::not_found(
                    K_ONLY_EXTRACT_LABELS,
                    spec,
                    "no mailbox with this label path; check spelling / parent path",
                ));
        }
        info!(
            event = "jmap_label_filter",
            requested = opts.only_mailbox_labels.len(),
            resolved_mailboxes = resolved.ids.len(),
            "resolved the label filter to mailboxes"
        );
        Some(resolved.ids.into_iter().collect())
    };
    // Every run, so a filter corrected or removed takes its rows with it.
    found.config(summary.problems.clone());

    // ── emails ──────────────────────────────────────────────────────
    bar.doing("emails");
    let listing = EmailListing {
        db,
        now: &now,
        session,
        account_id,
        opts,
    };
    let replayed = listing.replay_changes(&mut summary).await;
    let listed_any = listed::lists_any(db.pool()).await?;
    or_a_listing_problem(replayed, M_EMAIL_CHANGES, listed_any, opts, found)?;
    let enumerated = listing
        .enumerate_unlisted_scopes(mailbox_filter.as_ref(), &mut summary)
        .await;
    let listed_any = listed::lists_any(db.pool()).await?;
    or_a_listing_problem(enumerated, M_EMAIL_QUERY, listed_any, opts, found)?;
    listing
        .fetch_owed(mailbox_filter.as_ref(), bar, found, &mut summary)
        .await?;
    bar.did(1);

    // ── blobs ───────────────────────────────────────────────────────
    bar.doing("blobs");
    let blobs = sync_blobs(db, session, account_id, opts, bar, found, &mut summary).await;
    bar.did(1);
    blobs?;

    info!(
        event = "jmap_download_complete",
        mailboxes_upserted = summary.mailboxes_upserted,
        emails_upserted = summary.emails_upserted,
        emails_destroyed = summary.emails_destroyed,
        blobs_downloaded = summary.blobs_downloaded,
        blobs_oversize = summary.blobs_oversize,
        blobs_errored = summary.blobs_errored,
        "the JMAP download is done"
    );
    Ok(summary)
}

/// A listing that upstream would not answer is a `problems` row and the
/// run goes on with what an earlier listing `stored`; what it listed
/// before it failed stays listed, and nothing is deleted for being
/// absent from it. With nothing stored there is nothing to go on with,
/// and a store that would not write always fails the run.
fn or_a_listing_problem(
    listed: Result<()>,
    name: &str,
    stored: bool,
    opts: &FetchOptions,
    found: &RunProblems,
) -> Result<()> {
    let Err(e) = listed else {
        return Ok(());
    };
    if !api::is_upstream(&e) || !(stored || opts.control.stop.requested()) {
        return Err(e);
    }
    found.listing(name, format!("{e:#}"));
    Ok(())
}

// Mailboxes

async fn sync_mailboxes(
    db: &RawDb,
    now: &IsoOffsetTimestamp,
    session: &Session,
    account_id: &str,
    opts: &FetchOptions,
    summary: &mut FetchSummary,
) -> Result<()> {
    let stored = if opts.full_resync {
        None
    } else {
        db.load_state(account_id, "Mailbox").await?
    };

    if let Some(since) = stored {
        match incremental_mailboxes(db, now, session, account_id, &since, summary).await {
            Ok(()) => return Ok(()),
            Err(e) => warn!(
                event = "jmap_mailbox_changes_fallback",
                error = %e,
                "falling back to full Mailbox/get",
            ),
        }
    }

    // Full re-list.
    let resp = call(
        session,
        "Mailbox/get",
        json!({"accountId": account_id, "ids": null}),
    )
    .await?;
    let list = jmap_list(&resp);
    summary.mailboxes_upserted += list.len();
    upsert_mailboxes(db, now, account_id, &list).await?;
    // A full list names every mailbox the account has, so a row it did
    // not name is one upstream destroyed while we were not replaying
    // `Mailbox/changes`.
    let listed: HashSet<String> = list
        .iter()
        .filter_map(|m| m.get("id").and_then(Value::as_str).map(str::to_string))
        .collect();
    // An empty list is a server that answered oddly, not an account with
    // no Inbox; it must not strip every label off every email.
    let held = db.mailbox_names(account_id).await?;
    let gone = if listed.is_empty() {
        Vec::new()
    } else {
        unlisted_mailboxes(held.keys(), &listed)
    };
    summary.mailboxes_destroyed += gone.len();
    refile_mailboxes(db, now, &gone).await?;
    if let Some(state) = resp.get("state").and_then(|v| v.as_str()) {
        db.save_state(account_id, "Mailbox", state).await?;
    }
    Ok(())
}

async fn incremental_mailboxes(
    db: &RawDb,
    now: &IsoOffsetTimestamp,
    session: &Session,
    account_id: &str,
    since: &str,
    summary: &mut FetchSummary,
) -> Result<()> {
    let mut cursor = since.to_string();
    loop {
        let changes = call(
            session,
            "Mailbox/changes",
            json!({"accountId": account_id, "sinceState": cursor, "maxChanges": CHANGES_MAX}),
        )
        .await?;
        let created = string_array(&changes, "created");
        let updated = string_array(&changes, "updated");
        let destroyed = string_array(&changes, "destroyed");

        let to_fetch: Vec<String> = created.into_iter().chain(updated).collect();
        if !to_fetch.is_empty() {
            let resp = call(
                session,
                "Mailbox/get",
                json!({"accountId": account_id, "ids": to_fetch}),
            )
            .await?;
            let list = jmap_list(&resp);
            summary.mailboxes_upserted += list.len();
            upsert_mailboxes(db, now, account_id, &list).await?;
        }

        if !destroyed.is_empty() {
            summary.mailboxes_destroyed += destroyed.len();
            let moves: Vec<(String, Option<String>)> =
                destroyed.into_iter().map(|id| (id, None)).collect();
            refile_mailboxes(db, now, &moves).await?;
        }

        let new_state = changes
            .get("newState")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("Mailbox/changes response missing newState"))?
            .to_string();
        db.save_state(account_id, "Mailbox", &new_state).await?;

        let has_more = changes
            .get("hasMoreChanges")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if !has_more {
            return Ok(());
        }
        cursor = new_state;
    }
}

// Emails

/// What the email phase's three steps share.
struct EmailListing<'a> {
    db: &'a RawDb,
    now: &'a IsoOffsetTimestamp,
    session: &'a Session,
    account_id: &'a str,
    opts: &'a FetchOptions,
}

impl EmailListing<'_> {
    fn stopping(&self) -> bool {
        self.opts.control.stop.requested()
    }

    fn token_scope(&self) -> String {
        db::state_scope(self.account_id, "Email")
    }

    /// Replay `Email/changes` from the stored state, one response to a
    /// transaction: what it names as created or updated is listed under
    /// its `newState`, what it names as destroyed goes, and the state
    /// advances. Whether a named email has been fetched yet is not asked
    /// here, so the state never waits on a fetch.
    ///
    /// With no state to replay from (a first run, `full_resync`, or a
    /// state the server can no longer calculate changes from) the delta
    /// starts over from the account's state now.
    async fn replay_changes(&self, summary: &mut FetchSummary) -> Result<()> {
        let stored = if self.opts.full_resync {
            None
        } else {
            self.db.load_state(self.account_id, "Email").await?
        };
        let Some(mut since) = stored else {
            return self.start_over().await;
        };
        loop {
            if self.stopping() {
                return Ok(());
            }
            let changes = match call(
                self.session,
                "Email/changes",
                json!({"accountId": self.account_id, "sinceState": since, "maxChanges": CHANGES_MAX}),
            )
            .await
            {
                Ok(changes) => changes,
                Err(e) if api::cannot_calculate_changes(&e) => {
                    info!(
                        event = "jmap_email_state_expired",
                        state = %since,
                        "the server cannot calculate changes from the stored state; listing the account again"
                    );
                    return self.start_over().await;
                }
                Err(e) => return Err(e),
            };
            let named: Vec<String> = string_array(&changes, "created")
                .into_iter()
                .chain(string_array(&changes, "updated"))
                .collect();
            let destroyed = string_array(&changes, "destroyed");
            let new_state = changes
                .get("newState")
                .and_then(|v| v.as_str())
                .ok_or_else(|| anyhow!("Email/changes response missing newState"))?
                .to_string();

            let mut tx = self.db.pool().begin().await.context("begin changes tx")?;
            listed::list_in_tx(&mut tx, &named, Some(&new_state), Named::Changed).await?;
            summary.emails_destroyed +=
                listed::forget_in_tx(&mut tx, self.now, &destroyed, listed::same_id).await?;
            listed::save_token_in_tx(&mut tx, &self.token_scope(), &new_state).await?;
            tx.commit().await.context("commit changes tx")?;
            if let Some(sealer) = &self.opts.sealer {
                sealer.wrote(destroyed.len() as u64).await;
            }

            let has_more = changes
                .get("hasMoreChanges")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !has_more {
                return Ok(());
            }
            since = new_state;
        }
    }

    /// The state is asked for before anything is enumerated, so whatever
    /// changes while the enumeration walks is replayed by the next run.
    async fn start_over(&self) -> Result<()> {
        let resp = email_get(self.session, self.account_id, &[]).await?;
        let state = resp
            .get("state")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("Email/get response missing state"))?;
        listed::start_over(self.db.pool(), &self.token_scope(), Some(state), true).await
    }

    /// Enumerate, with `Email/query`, the admitted mailboxes (or the
    /// account) no enumeration has listed whole: a first run, a start
    /// over, a mailbox the label filter newly admits. Each page lists its
    /// ids; an email already listed is left as it is. The walk starts
    /// again every run until one reaches its end, and only that one
    /// records its scopes as listed whole and, when it covered the whole
    /// account, deletes what it did not name.
    async fn enumerate_unlisted_scopes(
        &self,
        mailbox_filter: Option<&BTreeSet<String>>,
        summary: &mut FetchSummary,
    ) -> Result<()> {
        let admitted: BTreeSet<String> = match mailbox_filter {
            None => [WHOLE_ACCOUNT.to_string()].into(),
            Some(ids) => ids.clone(),
        };
        let scopes = listed::scopes_owed(self.db.pool(), &admitted).await?;
        // Without a state there is no delta to say what changes after
        // the walk; the next run starts over and enumerates then.
        let Some(state) = self.db.load_state(self.account_id, "Email").await? else {
            return Ok(());
        };
        if scopes.is_empty() {
            return Ok(());
        }
        let filter = match (mailbox_filter, scopes.as_slice()) {
            (None, _) => Value::Null,
            (Some(_), [one]) => json!({"inMailbox": one}),
            (Some(_), many) => {
                let conds: Vec<Value> = many.iter().map(|m| json!({"inMailbox": m})).collect();
                json!({"operator": "OR", "conditions": conds})
            }
        };
        info!(
            event = "jmap_enumerating",
            scopes = %scopes.join(", "),
            "listing what no enumeration has listed whole"
        );

        let mut position: usize = 0;
        let mut query_state: Option<String> = None;
        // Every id any page listed. A restart after a `queryState` shift
        // keeps what it had: those emails existed when listed, and a
        // later destroy reaches us through `Email/changes`.
        let mut named: BTreeSet<String> = BTreeSet::new();
        loop {
            if self.stopping() {
                return Ok(());
            }
            let mut args = json!({
                "accountId": self.account_id,
                "sort": [{"property": "receivedAt", "isAscending": false}],
                "limit": EMAIL_QUERY_PAGE,
                "position": position,
                "calculateTotal": true,
            });
            if !filter.is_null() {
                args["filter"] = filter.clone();
            }
            let resp = call(self.session, "Email/query", args).await?;

            let page_state = resp
                .get("queryState")
                .and_then(|v| v.as_str())
                .map(String::from);
            if query_state.is_some() && page_state.is_some() && query_state != page_state {
                warn!(
                    event = "jmap_email_query_state_shift",
                    "queryState changed mid-pagination; restarting"
                );
                position = 0;
                query_state = page_state;
                continue;
            }
            query_state = query_state.or(page_state);

            let ids = string_array(&resp, "ids");
            if ids.is_empty() {
                break;
            }
            let mut tx = self.db.pool().begin().await.context("begin listing tx")?;
            listed::list_in_tx(&mut tx, &ids, Some(&state), Named::Exists).await?;
            tx.commit().await.context("commit listing tx")?;
            if let Some(sealer) = &self.opts.sealer {
                sealer.wrote(ids.len() as u64).await;
            }
            position += ids.len();
            named.extend(ids);

            let total = resp.get("total").and_then(|v| v.as_u64());
            if total.is_some_and(|total| position as u64 >= total) {
                break;
            }
        }
        let whole_account = mailbox_filter.is_none();
        summary.emails_destroyed += listed::close_enumeration(
            self.db.pool(),
            self.now,
            &scopes,
            whole_account.then_some(&named),
            listed::same_id,
        )
        .await?;
        Ok(())
    }

    /// Fetch, with `Email/get`, every listed email the store does not
    /// hold at its listed stamp: `owed::drain`, fifty to a batch. An
    /// email the server no longer has, or that a label filter keeps out,
    /// is gone: its listing, its row and whatever was held for it go.
    async fn fetch_owed(
        &self,
        mailbox_filter: Option<&BTreeSet<String>>,
        bar: &RunBar,
        found: &RunProblems,
        summary: &mut FetchSummary,
    ) -> Result<()> {
        let pool = self.db.pool();
        let owed = owed::owed(pool, listed::LISTED, listed::listing(pool, false).await?).await?;
        if owed.is_empty() {
            return Ok(());
        }
        bar.expect(owed.len() as u64);
        bar.doing("fetching emails");
        let f = EmailGet {
            session: self.session,
            account_id: self.account_id,
            mailbox_filter,
            now: self.now,
            bar,
            destroyed: AtomicUsize::new(0),
        };
        let l = owed::Loop {
            pool,
            table: listed::LISTED,
            phase: M_EMAIL_GET,
            stop: &self.opts.control.stop,
            found,
            sealer: self.opts.sealer.as_ref(),
            batch: EMAIL_GET_BATCH,
            concurrency: 1,
            flush: EMAIL_GET_BATCH,
            flush_bytes: 0,
            failures_in_a_row: GET_FAILURE_BUDGET,
        };
        let drained = owed::drain(&l, owed, &f).await?;
        summary.emails_upserted += drained.got;
        summary.emails_destroyed += f.destroyed.load(Ordering::Relaxed);
        Ok(())
    }
}

/// The emails phase's fetcher: one `Email/get` per batch of listed ids.
struct EmailGet<'a> {
    session: &'a Session,
    account_id: &'a str,
    mailbox_filter: Option<&'a BTreeSet<String>>,
    now: &'a IsoOffsetTimestamp,
    bar: &'a RunBar,
    destroyed: AtomicUsize,
}

#[async_trait]
impl Fetcher<EmailRow> for EmailGet<'_> {
    async fn fetch(
        &self,
        batch: Vec<Listed>,
    ) -> std::result::Result<Vec<Fetched<EmailRow>>, BatchError> {
        let ids: Vec<String> = batch.iter().map(|l| l.key.clone()).collect();
        let resp = email_get(self.session, self.account_id, &ids).await;
        self.bar.did(ids.len() as u64);
        match resp {
            Ok(resp) => Ok(answered(self.account_id, self.mailbox_filter, batch, &resp)),
            Err(e) if api::is_terminal(&e) => Err(BatchError::Terminal(e)),
            Err(e) => Err(BatchError::Batch(e)),
        }
    }

    async fn store(
        &self,
        tx: &mut Transaction<'static, Sqlite>,
        batch: &[Fetched<EmailRow>],
    ) -> Result<()> {
        let (got, gone) = got_and_gone(batch);
        let n = listed::write_batch_in_tx(tx, self.now, got, &gone, listed::same_id).await?;
        self.destroyed.fetch_add(n, Ordering::Relaxed);
        Ok(())
    }
}

/// What one `Email/get` says about each email `asked` for: an envelope
/// filed under an admitted mailbox is the email, one filed elsewhere or
/// named in `notFound` is gone, and the rest is left unanswered.
fn answered(
    account_id: &str,
    mailbox_filter: Option<&BTreeSet<String>>,
    asked: Vec<Listed>,
    resp: &Value,
) -> Vec<Fetched<EmailRow>> {
    let not_found = string_array(resp, "notFound");
    let mut rows: BTreeMap<String, EmailRow> = jmap_list(resp)
        .iter()
        .filter_map(|envelope| EmailRow::from_jmap_envelope(account_id, envelope))
        .map(|row| (row.id().to_string(), row))
        .collect();
    asked
        .into_iter()
        .filter_map(|listed| {
            let outcome = match rows.remove(&listed.key) {
                Some(row)
                    if mailbox_filter
                        .is_none_or(|f| row.mailbox_ids().iter().any(|m| f.contains(m))) =>
                {
                    Outcome::Got(row)
                }
                Some(_) => Outcome::Gone,
                None if not_found.contains(&listed.key) => Outcome::Gone,
                None => return None,
            };
            Some(Fetched { listed, outcome })
        })
        .collect()
}

/// A batch's answers split for [`listed::write_batch_in_tx`]: the rows
/// that came, and the ids of the messages that are gone.
fn got_and_gone(fetched: &[Fetched<EmailRow>]) -> (Vec<EmailRow>, Vec<String>) {
    let mut got = Vec::new();
    let mut gone = Vec::new();
    for f in fetched {
        match &f.outcome {
            Outcome::Got(row) | Outcome::Unusable(row, ..) => got.push(row.clone()),
            Outcome::Gone => gone.push(f.listed.key.clone()),
            Outcome::Failed(_) | Outcome::Skipped(..) => {}
        }
    }
    (got, gone)
}

async fn email_get(session: &Session, account_id: &str, ids: &[String]) -> Result<Value> {
    let props: Vec<Value> = EMAIL_GET_PROPERTIES
        .iter()
        .map(|s| Value::String((*s).to_string()))
        .collect();
    call(
        session,
        "Email/get",
        json!({
            "accountId": account_id,
            "ids": ids,
            "properties": props,
        }),
    )
    .await
}

// Blobs

/// Download the `.eml` of every email that has no stored bytes:
/// `owed::drain` over the edges, `blob_download_concurrency` at once,
/// each body into the CAS as it lands and its edge row written a flush
/// at a time. The email rows are already stored, so a download the
/// phase could not do leaves something to keep, and the next run
/// downloads whatever still has no bytes.
async fn sync_blobs(
    db: &RawDb,
    session: &Session,
    account_id: &str,
    opts: &FetchOptions,
    bar: &RunBar,
    found: &RunProblems,
    summary: &mut FetchSummary,
) -> Result<()> {
    if opts.control.stop.requested() {
        return Ok(());
    }
    let have_bytes = db.loaded_blob_ids().await?;
    summary.blobs_skipped = have_bytes.len();
    let mut jobs: HashMap<String, EmlJob> = HashMap::new();
    let mut listing = Vec::new();
    for em in db.load_emails().await? {
        if em.blob_id.is_empty() || have_bytes.contains_key(&em.blob_id) {
            continue;
        }
        let key = EmlBlobRow::pk_recipe(&em.id, &em.blob_id);
        listing.push(Listed::new(key.clone(), None::<String>));
        jobs.insert(
            key,
            EmlJob {
                email_id: em.id,
                blob_id: em.blob_id,
                advertised_size: em.size,
            },
        );
    }
    if listing.is_empty() {
        debug!(
            event = "jmap_blobs_up_to_date",
            "every blob is already stored"
        );
        return Ok(());
    }
    let concurrency = opts
        .blob_download_concurrency
        .unwrap_or(DEFAULT_BLOB_CONCURRENCY)
        .max(1);
    info!(
        event = "jmap_blobs_fetch",
        pending = listing.len(),
        concurrency,
        "fetching the pending blobs"
    );
    bar.expect(listing.len() as u64);
    bar.doing("fetching .eml");

    let f = EmlDownload {
        db,
        session,
        account_id,
        latchkey: &opts.latchkey,
        stop: &opts.control.stop,
        cap: opts.blob_size_limit_bytes,
        bar,
        jobs,
    };
    let l = owed::Loop {
        pool: db.pool(),
        table: EmlBlobRow::TABLE,
        phase: M_EML_DOWNLOAD,
        stop: &opts.control.stop,
        found,
        sealer: opts.sealer.as_ref(),
        batch: 1,
        concurrency,
        flush: opts.blob_flush_count.unwrap_or(BLOB_FLUSH_COUNT),
        flush_bytes: opts.blob_flush_bytes.unwrap_or(BLOB_FLUSH_BYTES),
        failures_in_a_row: BLOB_FAILURE_BUDGET,
    };
    let drained = owed::drain(&l, listing, &f).await?;
    summary.blobs_downloaded += drained.got;
    summary.blobs_oversize += drained.skipped;
    summary.blobs_errored += drained.failed;
    Ok(())
}

/// The blob phase's fetcher: one `.eml` per request, keyed by its edge
/// (`email_id#blob_id`); a flush's bytes go to the CAS in one write,
/// then the edge rows with their hashes.
struct EmlDownload<'a> {
    db: &'a RawDb,
    session: &'a Session,
    account_id: &'a str,
    latchkey: &'a LatchkeySettings,
    stop: &'a datalib_etl::stop::StopFlag,
    cap: Option<u64>,
    bar: &'a RunBar,
    jobs: HashMap<String, EmlJob>,
}

struct EmlJob {
    email_id: String,
    blob_id: String,
    advertised_size: Option<i64>,
}

/// A downloaded `.eml`: its bytes and the content type the server said.
struct Eml {
    bytes: Vec<u8>,
    content_type: Option<String>,
}

#[async_trait]
impl Fetcher<Eml> for EmlDownload<'_> {
    async fn fetch(
        &self,
        batch: Vec<Listed>,
    ) -> std::result::Result<Vec<Fetched<Eml>>, BatchError> {
        let mut answers = Vec::with_capacity(batch.len());
        for listed in batch {
            let Some(job) = self.jobs.get(&listed.key) else {
                continue;
            };
            let over_the_cap = self
                .cap
                .zip(job.advertised_size)
                .filter(|(cap, size)| *size as u64 > *cap);
            let outcome = if let Some((cap, size)) = over_the_cap {
                Outcome::Skipped(
                    datalib_problems::Reason::OverSizeLimit,
                    format!("the .eml is {size} bytes, over blob_size_limit_bytes ({cap})"),
                )
            } else {
                let url = self.session.download_url_for(
                    self.account_id,
                    &job.blob_id,
                    "message.eml",
                    "message/rfc822",
                );
                match api::download_bytes(&url, BLOB_TIMEOUT, self.latchkey).await {
                    Ok((bytes, content_type)) => Outcome::Got(Eml {
                        bytes,
                        content_type,
                    }),
                    Err(e) if self.stop.requested() => return Err(BatchError::Batch(e)),
                    Err(e) if api::is_terminal(&e) => return Err(BatchError::Terminal(e)),
                    Err(e) => Outcome::Failed(format!("{e:#}")),
                }
            };
            self.bar.did(1);
            answers.push(Fetched { listed, outcome });
        }
        Ok(answers)
    }

    async fn store(
        &self,
        tx: &mut Transaction<'static, Sqlite>,
        batch: &[Fetched<Eml>],
    ) -> Result<()> {
        let bodies: Vec<(&Eml, String)> = batch
            .iter()
            .filter_map(|f| match &f.outcome {
                Outcome::Got(eml) | Outcome::Unusable(eml, ..) => {
                    Some((eml, blake3_hex(&eml.bytes)))
                }
                Outcome::Gone | Outcome::Failed(_) | Outcome::Skipped(..) => None,
            })
            .collect();
        let inserts: Vec<CasInsert<'_>> = bodies
            .iter()
            .map(|(eml, blake3)| CasInsert {
                blake3,
                bytes: &eml.bytes,
                content_type: Some(eml.content_type.as_deref().unwrap_or("message/rfc822")),
            })
            .collect();
        self.db.cas().put_many(&inserts).await?;
        let mut hashes = bodies.iter().map(|(_, blake3)| blake3.clone());
        let rows: Vec<EmlBlobRow> = batch
            .iter()
            .filter_map(|f| {
                let job = self.jobs.get(&f.listed.key)?;
                let blake3 = match &f.outcome {
                    Outcome::Got(_) | Outcome::Unusable(..) => hashes.next(),
                    Outcome::Gone | Outcome::Failed(_) | Outcome::Skipped(..) => None,
                };
                Some(EmlBlobRow {
                    id: f.listed.key.clone(),
                    email_id: job.email_id.clone(),
                    blob_id: job.blob_id.clone(),
                    blake3,
                })
            })
            .collect();
        write_eml_edges_in_tx(tx, &rows).await
    }

    fn weight(&self, eml: &Eml) -> usize {
        eml.bytes.len()
    }
}

// Helpers

/// A JMAP `*/get` response's `list`, empty when it has none.
fn jmap_list(resp: &Value) -> Vec<Value> {
    resp.get("list")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default()
}

fn string_array(v: &Value, key: &str) -> Vec<String> {
    v.get(key)
        .and_then(|x| x.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn tmp_db() -> (tempfile::TempDir, RawDb) {
        let d = tempfile::tempdir().unwrap();
        let db = RawDb::open(&d.path().join("j.doltlite_db")).await.unwrap();
        (d, db)
    }

    fn now() -> IsoOffsetTimestamp {
        IsoOffsetTimestamp::now_local()
    }

    fn email(id: &str, mailboxes: &[&str]) -> EmailRow {
        let ids: serde_json::Map<String, Value> = mailboxes
            .iter()
            .map(|m| (m.to_string(), Value::Bool(true)))
            .collect();
        EmailRow::from_jmap_envelope(
            "A",
            &json!({"id": id, "blobId": "B", "threadId": "T", "mailboxIds": ids}),
        )
        .unwrap()
    }

    async fn payload_mailboxes(db: &RawDb, id: &str) -> Vec<String> {
        let p: String = sqlx::query_scalar("SELECT json(payload) FROM emails WHERE id = ?")
            .bind(id)
            .fetch_one(db.pool())
            .await
            .unwrap();
        let v: Value = serde_json::from_str(&p).unwrap();
        let mut ids: Vec<String> = v["mailboxIds"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        ids.sort();
        ids
    }

    async fn joined_mailboxes(db: &RawDb, id: &str) -> Vec<String> {
        let mut ids = db
            .load_email_joins()
            .await
            .unwrap()
            .mailboxes
            .remove(id)
            .unwrap_or_default();
        ids.sort();
        ids
    }

    /// The counts go to the sidecar, so a new message in the Inbox leaves
    /// the `mailboxes` row exactly as it was, and `dolt_diff_mailboxes`
    /// says nothing about it.
    #[tokio::test]
    async fn a_mailbox_count_changing_is_not_a_change_to_the_mailbox() {
        let (_d, db) = tmp_db().await;
        let inbox = |n: i64| json!({"id": "M1", "name": "Inbox", "role": "inbox", "totalEmails": n, "unreadEmails": n});
        upsert_mailboxes(&db, &now(), "A", &[inbox(1)])
            .await
            .unwrap();
        dr::commit_run(db.pool(), "one").await.unwrap();
        upsert_mailboxes(&db, &now(), "A", &[inbox(2)])
            .await
            .unwrap();
        dr::commit_run(db.pool(), "two").await.unwrap();

        let content: String = sqlx::query_scalar("SELECT json(payload) FROM mailboxes")
            .fetch_one(db.pool())
            .await
            .unwrap();
        assert!(!content.contains("totalEmails"), "{content}");
        let counts: String = sqlx::query_scalar(
            "SELECT json(volatile_payload) FROM mailboxes_bookkeeping WHERE id = 'M1'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&counts).unwrap(),
            json!({"totalEmails": 2, "unreadEmails": 2})
        );
        let changed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM dolt_diff_mailboxes WHERE from_ref = 'HEAD~1' AND to_ref = 'HEAD'",
        )
        .fetch_one(db.pool())
        .await
        .unwrap();
        assert_eq!(changed, 0);
        db.close().await;
    }

    /// A mailbox that went away upstream comes off every email that was
    /// in it — payload and join rows together — and its row goes.
    #[tokio::test]
    async fn refiling_to_nothing_takes_the_label_off_every_email() {
        let (_d, db) = tmp_db().await;
        upsert_mailboxes(
            &db,
            &now(),
            "A",
            &[
                json!({"id": "M1", "name": "Inbox"}),
                json!({"id": "M2", "name": "Work"}),
            ],
        )
        .await
        .unwrap();
        upsert_emails(
            &db,
            &now(),
            &[email("E1", &["M1", "M2"]), email("E2", &["M2"])],
        )
        .await
        .unwrap();

        let moved = refile_mailboxes(&db, &now(), &[("M2".into(), None)])
            .await
            .unwrap();

        assert_eq!(moved, 2);
        assert_eq!(payload_mailboxes(&db, "E1").await, vec!["M1"]);
        assert_eq!(joined_mailboxes(&db, "E1").await, vec!["M1"]);
        assert!(payload_mailboxes(&db, "E2").await.is_empty());
        assert!(joined_mailboxes(&db, "E2").await.is_empty());
        let names = db.mailbox_names("A").await.unwrap();
        assert_eq!(names.keys().collect::<Vec<_>>(), vec!["M1"]);
        db.close().await;
    }

    /// A row re-keyed onto a new id carries its emails with it.
    #[tokio::test]
    async fn refiling_onto_a_new_id_moves_every_email() {
        let (_d, db) = tmp_db().await;
        upsert_emails(&db, &now(), &[email("E1", &["old", "M1"])])
            .await
            .unwrap();

        refile_mailboxes(&db, &now(), &[("old".into(), Some("new".into()))])
            .await
            .unwrap();

        assert_eq!(payload_mailboxes(&db, "E1").await, vec!["M1", "new"]);
        assert_eq!(joined_mailboxes(&db, "E1").await, vec!["M1", "new"]);
        db.close().await;
    }

    #[test]
    fn a_full_listing_leaves_behind_the_rows_it_did_not_name() {
        let held = ["M1".to_string(), "M2".to_string(), "M3".to_string()];
        let listed: HashSet<String> = ["M1".to_string(), "M3".to_string()].into();
        assert_eq!(
            unlisted_mailboxes(held.iter(), &listed),
            vec![("M2".to_string(), None)]
        );
    }
}
