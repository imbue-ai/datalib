//! Gmail REST API downloader — the fourth mode of `type: email`.
//!
//! The preferred path for a Gmail account. It writes the same raw schema
//! as JMAP, IMAP, and mbox, so render is unchanged and a mailbox already
//! mirrored from a Google Takeout export dedupes against it rather than
//! doubling (see [`super::labels`] and [`super::envelope`] for the two
//! places that property is actually enforced).
//!
//! ## Why this and not IMAP, for Gmail
//!
//! * **No credential configuration.** latchkey's built-in `google-gmail`
//!   service routes by URL host, so the ordinary `latchkey curl` path
//!   injects and refreshes the token. One `latchkey auth browser
//!   google-gmail` and the config needs nothing but an empty stanza.
//! * **Deletions are reported.** `users.history.list` emits
//!   `messagesDeleted` explicitly. Gmail's IMAP advertises CONDSTORE but
//!   not QRESYNC, so there is no `VANISHED` there and deletions can only
//!   be found by re-listing every UID.
//! * **~13× the throughput.** IMAP is capped at 2500 MB/day of downloads;
//!   this is capped on quota units per minute — ~300 messages/minute
//!   regardless of size.
//!
//! ## Sync
//!
//! Cursor is the mailbox `historyId`, stored per account in the shared
//! `sync_scope_state` table — the same discipline as the JMAP path's
//! state tokens, under a `gmail:` key prefix instead of `jmap:`.
//!
//! * no cursor, or `full_resync` → full sync: `messages.list` paged,
//!   then `messages.get?format=RAW` per id.
//! * a cursor → `history.list`. `messagesAdded` and the relabeled ids are
//!   fetched; `messagesDeleted` hard-deletes the row (doltlite history
//!   retains the prior state), matching what the JMAP path does with
//!   `Email/changes` destroyed ids.
//! * `history.list` 404 means the cursor aged out of Google's retention
//!   window ("typically at least one week"). That is not an error — it is
//!   the documented signal to fall back to a full sync, exactly like
//!   JMAP's `cannotCalculateChanges`.

pub mod api;
pub mod ingest;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use anyhow::{Context, Result};
use datalib_etl::blob_cas::{CasEdgeAccumulator, CasEdgeRow as _};
use datalib_etl::control::DownloadControl;
use datalib_etl::progress::Progress;
use datalib_time::IsoOffsetTimestamp;
use serde::Serialize;
use serde_json::{json, Value};
use tracing::{info, warn};

use datalib_etl_email_config::EmailGmailApi;

use super::db::RawDb;
use super::schema_raw::{EmlBlobRow, ThreadRow};
use api::QuotaThrottle;
use ingest::LabelIndex;

/// `messages.list` page size. Google's maximum is 500; ids are tiny, so
/// there is no reason to ask for less.
const LIST_PAGE_SIZE: u32 = 500;
/// Flush accumulated rows every this many messages, so an interrupted run
/// keeps what it already fetched and peak memory stays bounded.
const FLUSH_BATCH: usize = 200;

#[derive(Debug, Clone)]
pub struct FetchOptions {
    pub db_path: PathBuf,
    pub db: Option<RawDb>,
    pub config: EmailGmailApi,
    /// When non-empty, only ingest messages carrying at least one label
    /// whose canonical path exactly matches one of these.
    pub only_labels: Vec<String>,
    pub blob_size_limit_bytes: Option<u64>,
    pub progress: Progress,
    pub control: DownloadControl,
}

impl Default for FetchOptions {
    fn default() -> Self {
        Self {
            db_path: PathBuf::new(),
            db: None,
            config: EmailGmailApi::default(),
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
    /// Gmail quota units spent, against the per-minute ceiling.
    pub quota_units_spent: u64,
    /// True when the run stopped at `message_budget` with more to fetch.
    /// A partial backfill is a successful outcome, not a failure.
    pub budget_exhausted: bool,
    /// True when a stored cursor had aged out and we re-enumerated.
    pub full_sync: bool,
}

/// Scope key for the Gmail API cursor. Namespaced like the JMAP path's
/// `jmap:` keys so several accounts can share one raw store.
fn state_scope(account_id: &str) -> String {
    format!("gmail:{account_id}:historyId")
}

pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let cfg = &opts.config;
    let account = cfg.account.as_deref();
    let user_id = cfg.user_id().to_string();

    let db = match opts.db.clone() {
        Some(db) => db,
        None => RawDb::open(&super::db::db_path_for(&opts.db_path)).await?,
    };
    if opts.control.reset_and_redownload {
        db.reset().await?;
    }

    let mut throttle = QuotaThrottle::new(cfg.quota_units_per_minute());
    let mut summary = FetchSummary::default();
    let now = IsoOffsetTimestamp::now_local().to_rfc3339();

    // ── account ─────────────────────────────────────────────────────
    throttle.acquire(api::UNITS_GET_PROFILE).await;
    let profile = api::get_profile(&user_id, account)
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
        &db,
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
    let index = LabelIndex::new(api::list_labels(&user_id, account).await?);
    let mailbox_payloads: Vec<Value> = index
        .mailboxes(&account_id)
        .into_iter()
        .map(|(id, name, role)| json!({ "id": id, "name": name, "role": role }))
        .collect();
    super::upsert_mailboxes(&db, &now, &account_id, &mailbox_payloads).await?;
    summary.mailboxes_upserted = mailbox_payloads.len();

    // ── decide full vs partial ──────────────────────────────────────
    let stored = if cfg.full_resync {
        None
    } else {
        db.load_scope(&state_scope(&account_id)).await?
    };
    let plan = match &stored {
        None => Plan::Full,
        Some(cursor) => {
            throttle.acquire(api::UNITS_HISTORY_LIST).await;
            match collect_history(&user_id, account, cursor, &mut throttle).await {
                Ok(changes) => Plan::Partial(changes),
                Err(e) if is_history_too_old(&e) => {
                    warn!(
                        event = "gmail_history_expired",
                        account = %account_id,
                        "stored historyId aged out of Google's retention window; re-enumerating",
                    );
                    Plan::Full
                }
                Err(e) => return Err(e),
            }
        }
    };

    // ── fetch ───────────────────────────────────────────────────────
    let mut state = RunState {
        db: &db,
        index: &index,
        account_id: &account_id,
        user_id: &user_id,
        account,
        now: &now,
        only_labels: opts.only_labels.iter().cloned().collect(),
        blob_size_limit_bytes: opts.blob_size_limit_bytes,
        budget: opts.config.message_budget,
        fetched: 0,
        threads: BTreeMap::new(),
        pending: Pending::default(),
    };

    match plan {
        Plan::Full => {
            summary.full_sync = true;
            full_sync(&mut state, &mut throttle, &opts, &mut summary).await?;
            // A full sync's cursor is the mailbox historyId sampled
            // *before* the enumeration, so anything that changed while it
            // ran is replayed next run rather than missed.
            if let Some(h) = &profile.history_id {
                db.save_scope(&state_scope(&account_id), h).await?;
            }
        }
        Plan::Partial(changes) => {
            summary.emails_destroyed = destroy(&db, &changes.deleted).await?;
            let ids: Vec<String> = changes
                .added
                .iter()
                .chain(changes.relabeled.iter())
                .cloned()
                .collect();
            info!(
                event = "gmail_partial_sync",
                account = %account_id,
                fetch = ids.len(),
                deleted = changes.deleted.len(),
                "history since stored cursor",
            );
            fetch_ids(&mut state, &mut throttle, &ids, &opts, &mut summary).await?;
            if let Some(h) = &changes.history_id {
                db.save_scope(&state_scope(&account_id), h).await?;
            }
        }
    }

    flush(&mut state, &mut summary).await?;
    flush_threads(&mut state, &mut summary).await?;
    summary.quota_units_spent = throttle.spent_total();
    Ok(summary)
}

enum Plan {
    Full,
    Partial(Changes),
}

#[derive(Debug, Default)]
struct Changes {
    added: Vec<String>,
    relabeled: Vec<String>,
    deleted: Vec<String>,
    history_id: Option<String>,
}

fn is_history_too_old(e: &anyhow::Error) -> bool {
    e.downcast_ref::<api::GmailApiError>()
        .is_some_and(|e| matches!(e, api::GmailApiError::HistoryTooOld))
}

/// Drain every page of `history.list` from `cursor`.
async fn collect_history(
    user_id: &str,
    account: Option<&str>,
    cursor: &str,
    throttle: &mut QuotaThrottle,
) -> Result<Changes> {
    let mut out = Changes::default();
    let mut token: Option<String> = None;
    loop {
        let page = api::list_history(user_id, account, cursor, token.as_deref()).await?;
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
    index: &'a LabelIndex,
    account_id: &'a str,
    user_id: &'a str,
    account: Option<&'a str>,
    now: &'a str,
    only_labels: BTreeSet<String>,
    blob_size_limit_bytes: Option<u64>,
    budget: Option<usize>,
    fetched: usize,
    threads: BTreeMap<String, Vec<(String, String)>>,
    pending: Pending,
}

#[derive(Default)]
struct Pending {
    emails: Vec<super::schema_raw::EmailRow>,
    cas: CasEdgeAccumulator,
    seen_blob_ids: BTreeSet<String>,
}

async fn full_sync(
    state: &mut RunState<'_>,
    throttle: &mut QuotaThrottle,
    opts: &FetchOptions,
    summary: &mut FetchSummary,
) -> Result<()> {
    let mut token: Option<String> = None;
    loop {
        throttle.acquire(api::UNITS_MESSAGES_LIST).await;
        let page = api::list_messages(
            state.user_id,
            state.account,
            token.as_deref(),
            LIST_PAGE_SIZE,
        )
        .await?;
        fetch_ids(state, throttle, &page.ids, opts, summary).await?;
        if summary.budget_exhausted {
            return Ok(());
        }
        match page.next_page_token {
            Some(t) => token = Some(t),
            None => return Ok(()),
        }
    }
}

async fn fetch_ids(
    state: &mut RunState<'_>,
    throttle: &mut QuotaThrottle,
    ids: &[String],
    opts: &FetchOptions,
    summary: &mut FetchSummary,
) -> Result<()> {
    let known = state.db.loaded_blob_ids().await?;
    for id in ids {
        if state.budget.is_some_and(|b| state.fetched >= b) {
            summary.budget_exhausted = true;
            info!(
                event = "gmail_budget_exhausted",
                fetched = state.fetched,
                "stopping early with a partial result; the cursor is committed",
            );
            return Ok(());
        }
        throttle.acquire(api::UNITS_MESSAGES_GET).await;
        let msg = match api::get_message_raw(state.user_id, state.account, id).await {
            Ok(m) => m,
            Err(e) => {
                // A message deleted between the list and the get is
                // normal on a busy mailbox, not a run-ending failure.
                warn!(event = "gmail_message_skipped", id = %id, error = %e);
                continue;
            }
        };
        state.fetched += 1;
        opts.progress.inc(1);

        let ingested = match ingest::ingest(state.account_id, state.index, &msg) {
            Ok(i) => i,
            Err(e) => {
                warn!(event = "gmail_ingest_failed", id = %msg.id, error = %e);
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
        } else if known.contains_key(&ingested.blob_id)
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

        state
            .threads
            .entry(ingested.thread_id.clone())
            .or_default()
            .push((ingested.email_id.clone(), ingested.received_at.clone()));
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
    Ok(())
}

/// One `threads` row per conversation, ordered by receipt time so render
/// gets a stable root message.
async fn flush_threads(state: &mut RunState<'_>, summary: &mut FetchSummary) -> Result<()> {
    let threads = std::mem::take(&mut state.threads);
    if threads.is_empty() {
        return Ok(());
    }
    let mut rows = Vec::with_capacity(threads.len());
    for (thread_id, mut members) in threads {
        members.sort_by(|a, b| a.1.cmp(&b.1).then_with(|| a.0.cmp(&b.0)));
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

/// Hard-delete the rows for messages Gmail reports as gone.
///
/// Matches the JMAP path's handling of `Email/changes` destroyed ids:
/// doltlite's history retains the prior state, so the row is recoverable
/// from a previous commit and does not need a tombstone.
///
/// Gmail's ids are per-transport, and our row ids are `Message-ID`-based,
/// so the mapping is not local — the deletion is applied by matching the
/// stored `_source.gmailMessageId` provenance stamped at ingest.
async fn destroy(db: &RawDb, gmail_ids: &[String]) -> Result<usize> {
    if gmail_ids.is_empty() {
        return Ok(0);
    }
    let mut destroyed = 0;
    for id in gmail_ids {
        let pattern = format!("%\"gmailMessageId\":\"{id}\"%");
        let affected = sqlx::query("DELETE FROM emails WHERE payload LIKE ?")
            .bind(&pattern)
            .execute(db.pool())
            .await
            .with_context(|| format!("deleting Gmail message {id}"))?
            .rows_affected();
        destroyed += affected as usize;
    }
    Ok(destroyed)
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

    /// A 404 from history.list is the documented "cursor aged out"
    /// signal, and must be recognized through anyhow's context chain —
    /// it arrives wrapped.
    #[test]
    fn recognizes_an_expired_cursor_through_context() {
        let e = anyhow::Error::new(api::GmailApiError::HistoryTooOld)
            .context("users.history.list")
            .context("while syncing");
        assert!(is_history_too_old(&e));
        assert!(!is_history_too_old(&anyhow::anyhow!("some other failure")));
    }
}
