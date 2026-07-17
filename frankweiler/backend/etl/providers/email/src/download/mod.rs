//! JMAP downloader. State-token-first incremental sync over four
//! phases:
//!
//!   1. Session — `.well-known/jmap` → cache `apiUrl`, `downloadUrl`,
//!      pick account, upsert the account row.
//!   2. Mailboxes — `Mailbox/changes` since stored state (full
//!      `Mailbox/get` on first run or when the server returns
//!      `cannotCalculateChanges`).
//!   3. Emails — `Email/changes` for created / updated / destroyed
//!      (full enumeration via `Email/query` as fallback). Detail via
//!      `Email/get` in batches; threadIds collected for the next phase.
//!      Destroyed ids hard-delete the row + joins + bookkeeping
//!      (dolt history preserves the prior state).
//!   4. Threads — `Thread/get` for every thread id touched this run.
//!   5. Blobs — `.eml` source per email + every
//!      `Email.attachments[].blobId`, fetched via the substituted
//!      `downloadUrl`. Respects `blob_size_limit_bytes`.
//!
//! State tokens are persisted per `(account_id, type_name)` in the
//! shared `sync_scope_state` table; see [`db::state_scope`].

pub mod api;
pub mod db;
pub mod mbox;
pub mod schema_raw;
pub mod session;

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use frankweiler_etl::blob_cas::{CasEdgeAccumulator, CasEdgeRow as _};
use frankweiler_etl::bulk::bulk_upsert_in_tx;
use frankweiler_etl::download_run::DownloadRun;
use frankweiler_time::IsoOffsetTimestamp;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::task::JoinSet;
use tracing::{debug, info, warn};

pub use db::{block_on_load_all, db_path_for, LoadedRaw, RawDb};

use api::call;
use db::refresh_email_joins;
use schema_raw::{AccountRow, EmailRow, EmlBlobRow, MailboxRow, ThreadRow};

/// Bulk-upsert one account row. Wraps the generic
/// `bulk_upsert_in_tx<AccountRow>` so call sites don't have to spell
/// out the row construction + tx ceremony.
///
/// `now` is the timestamp this fetch run stamps into every
/// `<table>_bookkeeping.fetched_at` it touches — see [`fetch`] for
/// where it's computed. Passing it down (rather than calling
/// `IsoOffsetTimestamp::now_local()` per upsert) gives a single
/// consistent "this run touched the row at" value across every
/// table written in one sync, and keeps the bookkeeping sidecars'
/// semantics honest: their stamp means "the sync that wrote this,"
/// not "the millisecond the upsert query ran."
async fn upsert_account(db: &RawDb, now: &str, id: &str, payload: &Value) -> Result<()> {
    let row = AccountRow::from_jmap_payload(id, payload)?;
    let mut tx = db.pool().begin().await.context("begin account tx")?;
    bulk_upsert_in_tx(&mut tx, std::slice::from_ref(&row), now).await?;
    tx.commit().await.context("commit account tx")?;
    Ok(())
}

/// Bulk-upsert a `Mailbox/get` `list` array under one account.
async fn upsert_mailboxes(
    db: &RawDb,
    now: &str,
    account_id: &str,
    payloads: &[Value],
) -> Result<()> {
    if payloads.is_empty() {
        return Ok(());
    }
    let rows: Vec<MailboxRow> = payloads
        .iter()
        .map(|p| MailboxRow::from_jmap_payload(account_id, p))
        .collect::<Result<Vec<_>>>()?;
    let mut tx = db.pool().begin().await.context("begin mailboxes tx")?;
    bulk_upsert_in_tx(&mut tx, &rows, now).await?;
    tx.commit().await.context("commit mailboxes tx")?;
    Ok(())
}

/// Bulk-upsert a batch of thread rows. Callers accumulate
/// `Vec<ThreadRow>` across whatever JMAP `Thread/get` page boundary
/// they're walking and flush once per batch — no per-row tx.
async fn upsert_threads(db: &RawDb, now: &str, rows: &[ThreadRow]) -> Result<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let mut tx = db.pool().begin().await.context("begin threads tx")?;
    bulk_upsert_in_tx(&mut tx, rows, now).await?;
    tx.commit().await.context("commit threads tx")?;
    Ok(())
}

/// Bulk-upsert a batch of emails: the envelope rows go through
/// `bulk_upsert_in_tx`, and each row's join tables get refreshed
/// (delete-then-insert) inside the same transaction.
async fn upsert_emails(db: &RawDb, now: &str, rows: &[EmailRow]) -> Result<()> {
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
/// Batch size for `Email/query` enumeration (full-resync fallback).
const EMAIL_QUERY_PAGE: usize = 500;
/// Batch size for `Thread/get`.
const THREAD_GET_BATCH: usize = 100;
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

#[derive(Debug, Clone, Default)]
pub struct FetchOptions {
    /// Either an explicit `.doltlite_db` file or the per-source directory;
    /// the shared `db_path_for` helper places the entity db inside as
    /// `entities.doltlite_db` (the dir is created if needed). Ignored
    /// for opening when `db` is `Some`.
    pub db_path: PathBuf,
    /// Pre-opened raw DB. When `Some`, `fetch` uses this directly
    /// instead of opening from `db_path`. Mirrors the pattern on the
    /// other providers' FetchOptions — sync opens the pool once for
    /// the post-download commit hook, then hands it back into fetch so
    /// the same writer process holds the file lock through the run.
    pub db: Option<RawDb>,
    pub hostname: String,
    pub account_id: Option<String>,
    /// Skip stored `state` tokens and re-enumerate via `Email/query`.
    /// Mailboxes still re-fetch via `Mailbox/get`.
    pub full_resync: bool,
    /// When non-empty, restrict the sync to mailboxes whose full label
    /// path (POSIX-like, e.g. `Work/Projects`; see
    /// [`crate::mailbox_labels`]) exactly matches one of these. Empty =
    /// every mailbox the account exposes. The paths are resolved to
    /// JMAP mailbox ids once `Mailbox/get` has run, then the filter is
    /// pushed server-side on full enumeration and applied client-side
    /// after `Email/get` on the incremental path (since `Email/changes`
    /// is account-scoped on JMAP).
    pub only_mailbox_labels: Vec<String>,
    /// Skip downloading any blob whose advertised size exceeds this.
    /// `None` = no limit.
    pub blob_size_limit_bytes: Option<u64>,
    /// How many `.eml` downloads to keep in flight at once during the
    /// blob phase. `None` → [`DEFAULT_BLOB_CONCURRENCY`]; clamped to ≥ 1.
    pub blob_download_concurrency: Option<usize>,
    pub progress: frankweiler_etl::progress::Progress,
    /// Cross-provider knobs (`--reset-and-redownload`, etc).
    pub control: frankweiler_etl::control::DownloadControl,
}

#[derive(Debug, Default, Clone, Serialize)]
pub struct FetchSummary {
    pub account_id: String,
    pub mailboxes_upserted: usize,
    pub mailboxes_destroyed: usize,
    pub emails_upserted: usize,
    pub emails_destroyed: usize,
    pub threads_upserted: usize,
    pub blobs_downloaded: usize,
    pub blobs_skipped: usize,
    pub blobs_errored: usize,
    pub blobs_oversize: usize,
}

/// Run one download pass against a JMAP account. Returns a summary the
/// orchestrator stamps into `sync_runs.summary`.
pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db = match opts.db.clone() {
        Some(d) => d,
        None => {
            let db_path = db_path_for(&opts.db_path);
            RawDb::open(&db_path).await?
        }
    };

    if opts.control.reset_and_redownload {
        db.reset().await?;
    }
    if opts.control.refetch_blobs {
        // Per-provider clear that sets every `email_blobs.blake3` back
        // to NULL so the next sync_blobs walk re-downloads every `.eml`
        // from scratch.
        db.clear_blob_hashes().await?;
    }

    // Coarse per-phase progress so the bar moves even though we don't
    // have a meaningful per-item denominator before the first JMAP
    // response. Without this, fastmail looks stuck at 0/0 in the
    // dashboard whether it's running or wedged on Session::discover.
    opts.progress.set_length(Some(5));
    opts.progress.set_message("email: session");

    let session = Session::discover(&opts.hostname)
        .await
        .with_context(|| format!("discover JMAP session at {}", opts.hostname))?;
    opts.progress.inc(1);
    let account_id = session.pick_account(opts.account_id.as_deref())?;
    info!(
        event = "jmap_session",
        hostname = %opts.hostname,
        account_id = %account_id,
        api_url = %session.api_url,
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

    let result = run_sync(&db, &session, &account_id, &opts).await;
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
) -> Result<FetchSummary> {
    let mut summary = FetchSummary {
        account_id: account_id.to_string(),
        ..Default::default()
    };

    // One timestamp per fetch run, threaded into every
    // `bulk_upsert_in_tx` call below. Goes into the bookkeeping
    // sidecars' `fetched_at` / `last_attempt_at` columns; the value
    // means "the sync that wrote this row," not "the millisecond the
    // UPSERT query ran" — so consistency across tables matters more
    // than sub-second freshness.
    let now = IsoOffsetTimestamp::now_local().to_rfc3339();

    // Persist the account row.
    let account_payload = session
        .accounts
        .iter()
        .find(|(k, _)| k == account_id)
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| json!({}));
    upsert_account(db, &now, account_id, &account_payload).await?;

    // ── mailboxes ───────────────────────────────────────────────────
    opts.progress.set_message("email: mailboxes");
    sync_mailboxes(db, &now, session, account_id, opts, &mut summary).await?;
    opts.progress.inc(1);

    // Resolve the configured label paths to mailbox ids now that the
    // full tree is in the db (`Mailbox/get` always re-lists, even on an
    // incremental run). Empty config = no filter (sync every mailbox).
    // An all-unmatched filter resolves to an empty set, which means
    // "match nothing" — loud-warned below so a typo'd path doesn't
    // silently drop the whole account.
    let mailbox_filter: Option<HashSet<String>> = if opts.only_mailbox_labels.is_empty() {
        None
    } else {
        let payloads = db.load_mailboxes().await?;
        let nodes: Vec<crate::mailbox_labels::MailboxNode> = payloads
            .iter()
            .filter_map(crate::mailbox_labels::MailboxNode::from_payload)
            .collect();
        let resolved = crate::mailbox_labels::resolve(&nodes, &opts.only_mailbox_labels);
        if !resolved.unmatched.is_empty() {
            warn!(
                event = "jmap_label_filter_unmatched",
                unmatched = ?resolved.unmatched,
                "only_extract_labels matched no mailbox; check spelling / parent path",
            );
        }
        info!(
            event = "jmap_label_filter",
            requested = opts.only_mailbox_labels.len(),
            resolved_mailboxes = resolved.ids.len(),
        );
        Some(resolved.ids)
    };

    // ── emails (+ collect threadIds) ────────────────────────────────
    opts.progress.set_message("email: emails");
    let touched_threads = sync_emails(
        db,
        &now,
        session,
        account_id,
        opts,
        mailbox_filter.as_ref(),
        &mut summary,
    )
    .await?;
    opts.progress.inc(1);

    // ── threads ─────────────────────────────────────────────────────
    opts.progress.set_message("email: threads");
    sync_threads(
        db,
        &now,
        session,
        account_id,
        &touched_threads,
        &mut summary,
    )
    .await?;
    opts.progress.inc(1);

    // ── blobs ───────────────────────────────────────────────────────
    opts.progress.set_message("email: blobs");
    sync_blobs(db, session, account_id, opts, &mut summary).await?;
    opts.progress.inc(1);

    info!(
        event = "jmap_download_complete",
        mailboxes_upserted = summary.mailboxes_upserted,
        emails_upserted = summary.emails_upserted,
        emails_destroyed = summary.emails_destroyed,
        threads_upserted = summary.threads_upserted,
        blobs_downloaded = summary.blobs_downloaded,
        blobs_oversize = summary.blobs_oversize,
        blobs_errored = summary.blobs_errored,
    );
    Ok(summary)
}

// ─────────────────────────────────────────────────────────────────────
// Mailboxes
// ─────────────────────────────────────────────────────────────────────

async fn sync_mailboxes(
    db: &RawDb,
    now: &str,
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
    let list = resp
        .get("list")
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    summary.mailboxes_upserted += list.len();
    upsert_mailboxes(db, now, account_id, &list).await?;
    if let Some(state) = resp.get("state").and_then(|v| v.as_str()) {
        db.save_state(account_id, "Mailbox", state).await?;
    }
    Ok(())
}

async fn incremental_mailboxes(
    db: &RawDb,
    now: &str,
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
            let list = resp
                .get("list")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            summary.mailboxes_upserted += list.len();
            upsert_mailboxes(db, now, account_id, &list).await?;
        }

        if !destroyed.is_empty() {
            summary.mailboxes_destroyed += destroyed.len();
            db.delete_mailboxes(&destroyed).await?;
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

// ─────────────────────────────────────────────────────────────────────
// Emails
// ─────────────────────────────────────────────────────────────────────

async fn sync_emails(
    db: &RawDb,
    now: &str,
    session: &Session,
    account_id: &str,
    opts: &FetchOptions,
    mailbox_filter: Option<&HashSet<String>>,
    summary: &mut FetchSummary,
) -> Result<HashSet<String>> {
    let stored = if opts.full_resync {
        None
    } else {
        db.load_state(account_id, "Email").await?
    };
    let mut touched_threads: HashSet<String> = HashSet::new();

    if let Some(since) = stored {
        match incremental_emails(
            db,
            now,
            session,
            account_id,
            &since,
            mailbox_filter,
            summary,
            &mut touched_threads,
        )
        .await
        {
            Ok(()) => return Ok(touched_threads),
            Err(e) => warn!(
                event = "jmap_email_changes_fallback",
                error = %e,
                "falling back to full Email/query enumeration",
            ),
        }
    }

    full_enumerate_emails(
        db,
        now,
        session,
        account_id,
        mailbox_filter,
        summary,
        &mut touched_threads,
    )
    .await?;
    Ok(touched_threads)
}

#[allow(clippy::too_many_arguments)]
async fn incremental_emails(
    db: &RawDb,
    now: &str,
    session: &Session,
    account_id: &str,
    since: &str,
    mailbox_filter: Option<&HashSet<String>>,
    summary: &mut FetchSummary,
    touched_threads: &mut HashSet<String>,
) -> Result<()> {
    let mut cursor = since.to_string();
    loop {
        let changes = call(
            session,
            "Email/changes",
            json!({"accountId": account_id, "sinceState": cursor, "maxChanges": CHANGES_MAX}),
        )
        .await?;
        let created = string_array(&changes, "created");
        let updated = string_array(&changes, "updated");
        let destroyed = string_array(&changes, "destroyed");

        // Detail-fetch created + updated in batches.
        let to_fetch: Vec<String> = created.into_iter().chain(updated).collect();
        for batch in to_fetch.chunks(EMAIL_GET_BATCH) {
            let resp = email_get(session, account_id, batch).await?;
            let list = resp
                .get("list")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            ingest_email_list(
                db,
                now,
                account_id,
                list,
                mailbox_filter,
                summary,
                touched_threads,
            )
            .await?;
        }

        if !destroyed.is_empty() {
            summary.emails_destroyed += destroyed.len();
            db.delete_emails(&destroyed).await?;
        }

        let new_state = changes
            .get("newState")
            .and_then(|v| v.as_str())
            .ok_or_else(|| anyhow!("Email/changes response missing newState"))?
            .to_string();
        db.save_state(account_id, "Email", &new_state).await?;

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

#[allow(clippy::too_many_arguments)]
async fn full_enumerate_emails(
    db: &RawDb,
    now: &str,
    session: &Session,
    account_id: &str,
    mailbox_filter: Option<&HashSet<String>>,
    summary: &mut FetchSummary,
    touched_threads: &mut HashSet<String>,
) -> Result<()> {
    // Decide filter: if a label filter resolved to mailbox ids, push it
    // server-side as an OR over inMailbox.
    let filter = match mailbox_filter {
        None => Value::Null,
        Some(set) if set.is_empty() => {
            // Label filter resolved to zero mailboxes (all paths
            // unmatched). Nothing can match — skip enumeration rather
            // than send a degenerate empty-OR filter.
            return Ok(());
        }
        Some(set) if set.len() == 1 => {
            json!({"inMailbox": set.iter().next().unwrap()})
        }
        Some(set) => {
            let conds: Vec<Value> = set.iter().map(|m| json!({"inMailbox": m})).collect();
            json!({"operator": "OR", "conditions": conds})
        }
    };

    let mut position: i64 = 0;
    let mut query_state: Option<String> = None;
    loop {
        let mut args = json!({
            "accountId": account_id,
            "sort": [{"property": "receivedAt", "isAscending": false}],
            "limit": EMAIL_QUERY_PAGE,
            "position": position,
            "calculateTotal": true,
        });
        if !filter.is_null() {
            args["filter"] = filter.clone();
        }
        let resp = call(session, "Email/query", args).await?;

        let page_state = resp
            .get("queryState")
            .and_then(|v| v.as_str())
            .map(String::from);
        if let (Some(stored), Some(current)) = (&query_state, &page_state) {
            if stored != current {
                // Result set shifted underneath us; restart from page 0.
                warn!(
                    event = "jmap_email_query_state_shift",
                    "queryState changed mid-pagination; restarting"
                );
                position = 0;
                query_state = Some(current.clone());
                continue;
            }
        } else if query_state.is_none() {
            query_state = page_state.clone();
        }

        let ids: Vec<String> = resp
            .get("ids")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if ids.is_empty() {
            break;
        }
        position += ids.len() as i64;

        for batch in ids.chunks(EMAIL_GET_BATCH) {
            let getresp = email_get(session, account_id, batch).await?;
            let list = getresp
                .get("list")
                .and_then(|v| v.as_array())
                .cloned()
                .unwrap_or_default();
            ingest_email_list(
                db,
                now,
                account_id,
                list,
                mailbox_filter,
                summary,
                touched_threads,
            )
            .await?;

            // The Email/get response's `state` is the live state token —
            // grab it on the *first* successful response and use it once
            // the full enumeration finishes.
            if let Some(state) = getresp.get("state").and_then(|v| v.as_str()) {
                db.save_state(account_id, "Email", state).await?;
            }
        }

        if let Some(total) = resp.get("total").and_then(|v| v.as_i64()) {
            if position >= total {
                break;
            }
        }
    }
    Ok(())
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

#[allow(clippy::too_many_arguments)]
async fn ingest_email_list(
    db: &RawDb,
    now: &str,
    account_id: &str,
    list: Vec<Value>,
    mailbox_filter: Option<&HashSet<String>>,
    summary: &mut FetchSummary,
    touched_threads: &mut HashSet<String>,
) -> Result<()> {
    let mut rows: Vec<EmailRow> = Vec::with_capacity(list.len());
    for envelope in list {
        let Some(row) = EmailRow::from_jmap_envelope(account_id, &envelope) else {
            continue;
        };
        if let Some(allow) = mailbox_filter {
            if !row.mailbox_ids().iter().any(|m| allow.contains(m)) {
                continue;
            }
        }
        touched_threads.insert(row.thread_id.clone());
        rows.push(row);
    }
    if rows.is_empty() {
        return Ok(());
    }
    summary.emails_upserted += rows.len();
    upsert_emails(db, now, &rows).await
}

// ─────────────────────────────────────────────────────────────────────
// Threads
// ─────────────────────────────────────────────────────────────────────

async fn sync_threads(
    db: &RawDb,
    now: &str,
    session: &Session,
    account_id: &str,
    touched: &HashSet<String>,
    summary: &mut FetchSummary,
) -> Result<()> {
    if touched.is_empty() {
        return Ok(());
    }
    let ids: Vec<String> = touched.iter().cloned().collect();
    for batch in ids.chunks(THREAD_GET_BATCH) {
        let resp = call(
            session,
            "Thread/get",
            json!({"accountId": account_id, "ids": batch}),
        )
        .await?;
        let list = resp
            .get("list")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        // Build the whole batch's worth of rows up front, then bulk-
        // upsert in one tx. The per-thread `upsert_thread` call this
        // replaced opened a fresh transaction per row, which made a
        // 200-thread sync 200 sequential commits.
        let mut rows: Vec<ThreadRow> = Vec::with_capacity(list.len());
        for thread in &list {
            let Some(id) = thread.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            rows.push(ThreadRow::from_jmap_payload(id, account_id, thread)?);
        }
        summary.threads_upserted += rows.len();
        upsert_threads(db, now, &rows).await?;
        if let Some(state) = resp.get("state").and_then(|v| v.as_str()) {
            db.save_state(account_id, "Thread", state).await?;
        }
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────
// Blobs
// ─────────────────────────────────────────────────────────────────────

/// Download the `.eml` for every email that doesn't have its
/// blake3 set yet. After the eml-as-canonical port we no longer
/// fetch attachments separately — the `.eml` is the complete
/// backup, render mail-parses parts on demand.
async fn sync_blobs(
    db: &RawDb,
    session: &Session,
    account_id: &str,
    opts: &FetchOptions,
    summary: &mut FetchSummary,
) -> Result<()> {
    let have_bytes = db.loaded_blob_ids().await?;

    // Build the to-fetch worklist: per-email `.eml` source only.
    // BTreeMap by blob_id dedupes (multiple emails can share the
    // same blob_id in theory) and gives stable dispatch order.
    let mut wanted: BTreeMap<String, EmlJob> = BTreeMap::new();
    for em in db.load_emails().await? {
        if em.blob_id.is_empty() || have_bytes.contains_key(&em.blob_id) {
            continue;
        }
        wanted.insert(
            em.blob_id.clone(),
            EmlJob {
                owning_id: em.id.clone(),
                advertised_size: em.size,
            },
        );
    }

    summary.blobs_skipped = have_bytes.len();
    if wanted.is_empty() {
        debug!(event = "jmap_blobs_up_to_date");
        return Ok(());
    }
    info!(event = "jmap_blobs_pending", count = wanted.len());

    // Accumulate downloads + their `email_blobs` edges in the shared
    // CAS-edge accumulator: each fetched `.eml` carries its bytes (for
    // the end-of-pass `put_many`) and yields an edge whose `blake3` the
    // accumulator resolves off those bytes. Failures get an edge with
    // NULL `blake3` and an error stamp on `email_blobs_bookkeeping`.
    let mut acc = CasEdgeAccumulator::new();

    // Split the worklist: oversize `.eml`s are recorded as failures up
    // front (no GET), the rest become owned download jobs. The oversize
    // check is cheap and serial; only the network GETs fan out.
    let mut jobs: Vec<(String, String)> = Vec::new();
    for (blob_id, job) in wanted {
        if let Some(limit) = opts.blob_size_limit_bytes {
            if let Some(sz) = job.advertised_size {
                if sz as u64 > limit {
                    summary.blobs_oversize += 1;
                    acc.add_failed(
                        &job.owning_id,
                        &blob_id,
                        format!(".eml {blob_id} exceeds size_limit {limit}"),
                    );
                    continue;
                }
            }
        }
        jobs.push((blob_id, job.owning_id));
    }

    // Bounded fan-out. JMAP exposes no bulk-blob method, so each `.eml`
    // is its own GET; the win on a large backfill is having up to
    // `concurrency` of them in flight at once. The downloads run on the
    // runtime while this single task drains completions and feeds the
    // accumulator — so `acc` mutation stays serial and lock-free even
    // though the network I/O is concurrent.
    let concurrency = opts
        .blob_download_concurrency
        .unwrap_or(DEFAULT_BLOB_CONCURRENCY)
        .max(1);
    info!(
        event = "jmap_blobs_fetch",
        pending = jobs.len(),
        concurrency
    );

    // Inner per-`.eml` bar nested under the outer phase bar (which only
    // ticks once for the whole blob phase). The worklist is fully
    // materialized, so we know the exact total up front and can render
    // real N/total progress as downloads complete.
    let inner = opts.progress.child("email: blobs");
    inner.set_length(Some(jobs.len() as u64));
    inner.set_message("fetching .eml");

    // Build a download task from an owned (blob_id, owning_id). The
    // `downloadUrl` is substituted here (borrowing `session`) so the
    // spawned future owns only `String`s and is `Send + 'static`.
    let spawn_one = |set: &mut JoinSet<EmlFetchOutcome>, blob_id: String, owning_id: String| {
        let url = session.download_url_for(account_id, &blob_id, "message.eml", "message/rfc822");
        set.spawn(async move {
            let result = api::download_bytes(&url, BLOB_TIMEOUT).await;
            (blob_id, owning_id, result)
        });
    };

    let mut pending = jobs.into_iter();
    let mut set: JoinSet<EmlFetchOutcome> = JoinSet::new();
    for _ in 0..concurrency {
        match pending.next() {
            Some((blob_id, owning_id)) => spawn_one(&mut set, blob_id, owning_id),
            None => break,
        }
    }

    while let Some(joined) = set.join_next().await {
        let (blob_id, owning_id, result) = joined.context("blob download task panicked")?;
        match result {
            Ok((bytes, content_type)) => {
                acc.add_fetched(
                    &owning_id,
                    &blob_id,
                    bytes,
                    Some(content_type.unwrap_or_else(|| "message/rfc822".to_string())),
                    None,
                );
                summary.blobs_downloaded += 1;
            }
            Err(e) => {
                summary.blobs_errored += 1;
                warn!(event = "jmap_blob_error", blob_id = %blob_id, error = %e);
                acc.add_failed(&owning_id, &blob_id, e.to_string());
            }
        }
        inner.inc(1);
        // Backfill the freed slot so `concurrency` GETs stay in flight.
        if let Some((blob_id, owning_id)) = pending.next() {
            spawn_one(&mut set, blob_id, owning_id);
        }
    }
    inner.finish_and_clear();

    acc.flush(db.pool(), db.cas(), |email_id, blob_id, blake3| {
        EmlBlobRow {
            id: EmlBlobRow::pk_recipe(email_id, blob_id),
            email_id: email_id.to_string(),
            blob_id: blob_id.to_string(),
            blake3: blake3.map(str::to_string),
        }
    })
    .await?;
    Ok(())
}

struct EmlJob {
    owning_id: String,
    advertised_size: Option<i64>,
}

/// One blob download task's result: `(blob_id, owning_id, bytes-or-err)`.
/// The ids ride along so the draining loop can route the outcome to the
/// accumulator without tracking which task was which.
type EmlFetchOutcome = (String, String, Result<(Vec<u8>, Option<String>)>);

// ─────────────────────────────────────────────────────────────────────
// Helpers
// ─────────────────────────────────────────────────────────────────────

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
