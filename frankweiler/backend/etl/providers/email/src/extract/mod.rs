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
pub mod session;

use std::collections::{BTreeMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use frankweiler_etl::extract_run::ExtractRun;
use serde::Serialize;
use serde_json::{json, Value};
use tracing::{debug, info, warn};

pub use db::{block_on_load_all, db_path_for, LoadedRaw, RawDb};

use api::call;
use db::{EmailRow, BLOB_KIND_ATTACHMENT, BLOB_KIND_EML};
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

/// Email/get properties we ask the server for. Includes the structural
/// body refs (`bodyValues`, `textBody`, `htmlBody`) plus the headers we
/// promote into typed columns. Bodies up to `maxBodyValueBytes` arrive
/// inline; anything truncated remains available via the `.eml` blob.
const EMAIL_GET_PROPERTIES: &[&str] = &[
    "id",
    "blobId",
    "threadId",
    "mailboxIds",
    "keywords",
    "from",
    "to",
    "cc",
    "bcc",
    "replyTo",
    "subject",
    "sentAt",
    "receivedAt",
    "size",
    "messageId",
    "inReplyTo",
    "references",
    "hasAttachment",
    "attachments",
    "preview",
    "bodyValues",
    "textBody",
    "htmlBody",
    "headers",
];

const EMAIL_BODY_VALUE_MAX_BYTES: u64 = 1_000_000;

#[derive(Debug, Clone, Default)]
pub struct FetchOptions {
    /// Either an explicit `.doltlite_db` file or a parent directory; the
    /// shared `db_path_for` helper rewrites it consistently. Ignored
    /// for opening when `db` is `Some`.
    pub db_path: PathBuf,
    /// Pre-opened raw DB. When `Some`, `fetch` uses this directly
    /// instead of opening from `db_path`. Mirrors the pattern on the
    /// other providers' FetchOptions — sync opens the pool once for
    /// the post-extract commit hook, then hands it back into fetch so
    /// the same writer process holds the file lock through the run.
    pub db: Option<RawDb>,
    pub hostname: String,
    pub account_id: Option<String>,
    /// Skip stored `state` tokens and re-enumerate via `Email/query`.
    /// Mailboxes still re-fetch via `Mailbox/get`.
    pub full_resync: bool,
    /// When non-empty, only emails whose `mailboxIds` intersect this
    /// set get persisted. Implemented client-side after `Email/get`,
    /// since `Email/changes` is account-scoped on JMAP.
    pub only_mailbox_ids: Vec<String>,
    /// Skip downloading any blob whose advertised size exceeds this.
    /// `None` = no limit.
    pub blob_size_limit_bytes: Option<u64>,
    pub progress: frankweiler_etl::progress::Progress,
    /// Cross-provider knobs (`--reset-and-redownload`, etc).
    pub control: frankweiler_etl::control::ExtractControl,
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

/// Run one extract pass against a JMAP account. Returns a summary the
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
    let run = ExtractRun::start(
        db.pool(),
        &json!({
            "hostname": opts.hostname,
            "account_id": account_id,
            "full_resync": opts.full_resync,
            "only_mailbox_ids": opts.only_mailbox_ids,
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

    // Persist the account row.
    let account_payload = session
        .accounts
        .iter()
        .find(|(k, _)| k == account_id)
        .map(|(_, v)| v.clone())
        .unwrap_or_else(|| json!({}));
    db.upsert_account(account_id, &account_payload).await?;

    let mailbox_filter: Option<HashSet<String>> = if opts.only_mailbox_ids.is_empty() {
        None
    } else {
        Some(opts.only_mailbox_ids.iter().cloned().collect())
    };

    // ── mailboxes ───────────────────────────────────────────────────
    opts.progress.set_message("email: mailboxes");
    sync_mailboxes(db, session, account_id, opts, &mut summary).await?;
    opts.progress.inc(1);

    // ── emails (+ collect threadIds) ────────────────────────────────
    opts.progress.set_message("email: emails");
    let touched_threads = sync_emails(
        db,
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
    sync_threads(db, session, account_id, &touched_threads, &mut summary).await?;
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
        match incremental_mailboxes(db, session, account_id, &since, summary).await {
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
    db.upsert_mailboxes(account_id, &list).await?;
    if let Some(state) = resp.get("state").and_then(|v| v.as_str()) {
        db.save_state(account_id, "Mailbox", state).await?;
    }
    Ok(())
}

async fn incremental_mailboxes(
    db: &RawDb,
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
            db.upsert_mailboxes(account_id, &list).await?;
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
        session,
        account_id,
        mailbox_filter,
        summary,
        &mut touched_threads,
    )
    .await?;
    Ok(touched_threads)
}

async fn incremental_emails(
    db: &RawDb,
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

async fn full_enumerate_emails(
    db: &RawDb,
    session: &Session,
    account_id: &str,
    mailbox_filter: Option<&HashSet<String>>,
    summary: &mut FetchSummary,
    touched_threads: &mut HashSet<String>,
) -> Result<()> {
    // Decide filter: if only_mailbox_ids is set, push it server-side
    // as an OR over inMailbox.
    let filter = match mailbox_filter {
        None => Value::Null,
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
        if let (Some(stored), Some(now)) = (&query_state, &page_state) {
            if stored != now {
                // Result set shifted underneath us; restart from page 0.
                warn!(
                    event = "jmap_email_query_state_shift",
                    "queryState changed mid-pagination; restarting"
                );
                position = 0;
                query_state = Some(now.clone());
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
            "fetchTextBodyValues": true,
            "fetchHTMLBodyValues": true,
            "maxBodyValueBytes": EMAIL_BODY_VALUE_MAX_BYTES,
        }),
    )
    .await
}

async fn ingest_email_list(
    db: &RawDb,
    account_id: &str,
    list: Vec<Value>,
    mailbox_filter: Option<&HashSet<String>>,
    summary: &mut FetchSummary,
    touched_threads: &mut HashSet<String>,
) -> Result<()> {
    let mut rows: Vec<EmailRow> = Vec::with_capacity(list.len());
    for payload in list {
        let Some(row) = EmailRow::from_payload(account_id, payload) else {
            continue;
        };
        if let Some(allow) = mailbox_filter {
            if !row.mailbox_ids.iter().any(|m| allow.contains(m)) {
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
    db.upsert_emails(&rows).await
}

// ─────────────────────────────────────────────────────────────────────
// Threads
// ─────────────────────────────────────────────────────────────────────

async fn sync_threads(
    db: &RawDb,
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
        for thread in list {
            let Some(id) = thread.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            db.upsert_thread(id, account_id, &thread).await?;
            summary.threads_upserted += 1;
        }
        if let Some(state) = resp.get("state").and_then(|v| v.as_str()) {
            db.save_state(account_id, "Thread", state).await?;
        }
    }
    Ok(())
}

// ─────────────────────────────────────────────────────────────────────
// Blobs
// ─────────────────────────────────────────────────────────────────────

async fn sync_blobs(
    db: &RawDb,
    session: &Session,
    account_id: &str,
    opts: &FetchOptions,
    summary: &mut FetchSummary,
) -> Result<()> {
    let have_bytes = db.loaded_blob_ids().await?;

    // Per-email .eml source + per-attachment blobs. We scan the emails
    // table for what's needed; the rows hold the blobId and the
    // attachments join table holds the per-part blobs + advertised
    // sizes for the size-limit gate.
    //
    // BTreeMap so the dispatch order is stable; helps reading logs.
    let mut wanted: BTreeMap<String, BlobJob> = BTreeMap::new();

    // .eml source per email.
    for em in db.load_emails().await? {
        if em.blob_id.is_empty() || have_bytes.contains(&em.blob_id) {
            continue;
        }
        wanted.insert(
            em.blob_id.clone(),
            BlobJob {
                kind: BLOB_KIND_EML,
                owning_id: em.id.clone(),
                slot: "source".to_string(),
                advertised_size: em.size,
                name: "message.eml".to_string(),
                content_type: "message/rfc822".to_string(),
            },
        );
    }

    // Attachments.
    let joins = db.load_email_joins().await?;
    for (email_id, atts) in &joins.attachments {
        for a in atts {
            if a.blob_id.is_empty() || have_bytes.contains(&a.blob_id) {
                continue;
            }
            wanted.insert(
                a.blob_id.clone(),
                BlobJob {
                    kind: BLOB_KIND_ATTACHMENT,
                    owning_id: email_id.clone(),
                    slot: a.part_id.clone(),
                    advertised_size: a.size,
                    name: a.name.clone().unwrap_or_else(|| "blob".to_string()),
                    content_type: a
                        .content_type
                        .clone()
                        .unwrap_or_else(|| "application/octet-stream".to_string()),
                },
            );
        }
    }

    if wanted.is_empty() {
        debug!(event = "jmap_blobs_up_to_date");
        return Ok(());
    }
    info!(event = "jmap_blobs_pending", count = wanted.len());

    for (blob_id, job) in wanted {
        if let Some(limit) = opts.blob_size_limit_bytes {
            if let Some(sz) = job.advertised_size {
                if sz as u64 > limit {
                    summary.blobs_oversize += 1;
                    // Pre-seed a stub row so the bookkeeping shows we
                    // know about it; the blake3 column stays NULL.
                    db.pre_seed_blob_stub(
                        &blob_id,
                        job.kind,
                        &job.owning_id,
                        &job.slot,
                        Some(&job.content_type),
                        None,
                    )
                    .await?;
                    continue;
                }
            }
        }

        let url = session.download_url_for(account_id, &blob_id, &job.name, &job.content_type);
        match api::download_bytes(&url, BLOB_TIMEOUT).await {
            Ok((bytes, content_type)) => {
                let ct = content_type.as_deref().unwrap_or(job.content_type.as_str());
                db.store_blob(
                    &frankweiler_etl::blob_cas::RefStub {
                        ref_id: &blob_id,
                        kind: job.kind,
                        owning_id: &job.owning_id,
                        slot: &job.slot,
                        upstream_uuid: Some(&blob_id),
                        upstream_name: Some(job.name.as_str()),
                        source_url: Some(&url),
                        content_type: Some(ct),
                    },
                    &bytes,
                )
                .await?;
                summary.blobs_downloaded += 1;
            }
            Err(e) => {
                summary.blobs_errored += 1;
                warn!(event = "jmap_blob_error", blob_id = %blob_id, error = %e);
                db.record_blob_error(&blob_id, &job.owning_id, &job.slot, &e.to_string())
                    .await?;
            }
        }
    }
    let _ = summary.blobs_skipped; // populated when we know-skip via cache later
    Ok(())
}

struct BlobJob {
    kind: &'static str,
    owning_id: String,
    slot: String,
    advertised_size: Option<i64>,
    name: String,
    content_type: String,
}

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
