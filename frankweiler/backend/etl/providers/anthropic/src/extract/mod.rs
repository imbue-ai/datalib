//! Anthropic (claude.ai) downloader entry point. Port of
//! `src/download/claude_web.py`.
//!
//! Writes into a single doltlite database file
//! (`<data_root>/<name>/raw/entities.doltlite_db`). Conversations are stored as
//! the **raw** `/api/...` payload — the export-shape normalization
//! used to happen here at fetch time, but now lives in `translate`
//! so the raw store stays as close to the wire as possible.

pub mod api;
pub mod db;
pub mod normalize;
pub mod schema_raw;

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result};
use frankweiler_etl::bulk::bulk_upsert_in_tx;
use frankweiler_etl::doltlite_raw::WirePayload;
use frankweiler_etl::extract_run::ExtractRun;
use frankweiler_etl::http::{latchkey_curl, HttpRequest};
use frankweiler_time::IsoOffsetTimestamp;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::time::sleep;
use tracing::{info, info_span, instrument, warn, Instrument};

pub use api::{ClaudeClient, ClaudeError};
pub use db::{db_path_for, LoadedConversation, LoadedRaw, RawDb};
use frankweiler_etl::blob_cas::CasEdgeRow as _;
use schema_raw::{
    ConversationAttachmentRow, ConversationRow as ConversationRowSchema, OrgRow, UserRow,
};

pub const SLEEP_BETWEEN: Duration = Duration::from_millis(400);
pub const DEFAULT_OVERLAP: usize = 3;
const ATTACH_FILE_TIMEOUT: Duration = Duration::from_secs(600);
const CLAUDE_ORIGIN: &str = "https://claude.ai";

#[derive(Debug, Clone, Default)]
pub struct FetchOptions {
    /// Path to the doltlite database file. The entity db lives inside
    /// the per-source directory as `entities.doltlite_db` (the dir is
    /// created if needed). Ignored for opening when `db` is `Some`.
    pub db_path: PathBuf,
    /// Pre-opened raw DB. When `Some`, `fetch` uses this directly
    /// instead of opening from `db_path`. The sync orchestrator pre-
    /// opens at startup so a download isn't started against a DB we
    /// can't write to (and so the post-extract commit can run on the
    /// same connection — no reopen race).
    pub db: Option<RawDb>,
    /// Path to a bulk-export directory (`users.json` and friends). If
    /// set and the DB is missing users, we pre-seed them from here.
    pub export_dir: Option<PathBuf>,
    pub overlap: usize,
    pub sleep_between: Duration,
    /// When non-empty, fetch only these conversation UUIDs. The
    /// listing walk is skipped entirely.
    pub conv_uuids: Vec<String>,
    pub progress: frankweiler_etl::progress::Progress,
    /// Cross-provider knobs (`--reset-and-redownload`, etc).
    pub control: frankweiler_etl::control::ExtractControl,
}

#[derive(Debug, Default, Serialize)]
pub struct FetchSummary {
    pub fetched: usize,
    pub skipped: usize,
    pub forbidden_orgs: usize,
    pub errors: usize,
    pub total: usize,
    pub new_blobs: usize,
    pub skipped_blobs: usize,
    pub failed_blobs: usize,
    pub requests: u64,
    pub network_seconds: f64,
    /// Total number of extra `get_conversation` attempts spent on
    /// transient-403 retries (does not count the initial attempt).
    pub forbidden_retry_attempts: u64,
    /// Conversations that ultimately succeeded only after at least one
    /// retry. `forbidden_retry_attempts > 0` with `_recovered == 0`
    /// would mean every retry path exhausted without success.
    pub forbidden_retry_recoveries: u64,
}

#[instrument(skip_all, fields(db = %opts.db_path.display()))]
pub async fn fetch(opts: FetchOptions) -> Result<FetchSummary> {
    let db_path = db_path_for(&opts.db_path);
    let _ = frankweiler_etl::latchkey::ensure_curl_shim();
    let db = match opts.db.clone() {
        Some(db) => db,
        None => RawDb::open(&db_path)
            .await
            .with_context(|| format!("open raw db {}", db_path.display()))?,
    };

    if opts.control.reset_and_redownload {
        info!(event = "anthropic_reset_and_redownload");
        db.reset().await.context("reset raw db before redownload")?;
    }
    if opts.control.refetch_blobs {
        info!(event = "anthropic_refetch_blobs");
        db.clear_blob_hashes()
            .await
            .context("clear anthropic_attachments.blake3 before refetch")?;
    }

    let run_config = json!({
        "overlap": opts.overlap,
        "conv_uuids": opts.conv_uuids,
    });
    let run = ExtractRun::start(db.pool(), &run_config).await?;
    let mut client = ClaudeClient::new();
    let mut summary = FetchSummary::default();
    // One `now` per fetch — threaded into every bulk upsert so all
    // `<table>_bookkeeping.fetched_at` stamps from a single sync share
    // a timestamp.
    let now = IsoOffsetTimestamp::now_local().to_rfc3339();
    // Run-scoped `(file_uuid → blake3)` cache, loaded once up-front
    // so the per-file dedupe check inside `fetch_files_for` is a
    // HashMap hit instead of a SQLite round trip. Successful
    // downloads insert into it.
    let mut blake3_by_file = db.load_attachment_blake3s().await?;

    let work = async {
        // users.json from the bulk export carries the account.uuid we
        // need on every conversation. If the DB doesn't have any user
        // yet, try to pull it from the export dir before falling back
        // to `/api/account`.
        if !db.has_any_user().await.unwrap_or(false) {
            if let Some(export_dir) = opts.export_dir.as_deref() {
                ingest_export_users(&db, export_dir, &now)
                    .await
                    .unwrap_or_else(|e| {
                        warn!(event = "anthropic_export_users_failed", error = %e);
                    });
            }
        }
        if !db.has_any_user().await.unwrap_or(false) {
            match client.current_account().await {
                Ok(acct) => {
                    let entry = pick_user_fields(&acct);
                    if let Err(e) = upsert_users(&db, &[entry], &now).await {
                        warn!(event = "anthropic_synthesize_user_failed", error = %e);
                    } else {
                        info!(event = "anthropic_users_synthesized");
                    }
                }
                Err(e) => warn!(
                    event = "anthropic_current_account_failed",
                    error = %e,
                    note = "users will be empty"
                ),
            }
        }

        let orgs = client
            .list_orgs()
            .await
            .map_err(|e| anyhow::anyhow!("list orgs: {e}"))?;
        info!(event = "anthropic_orgs", count = orgs.len());
        if let Err(e) = upsert_orgs(&db, &orgs, &now).await {
            warn!(event = "anthropic_orgs_upsert_failed", error = %e);
        }

        if !opts.conv_uuids.is_empty() {
            opts.progress.set_length(Some(opts.conv_uuids.len() as u64));
            for raw in &opts.conv_uuids {
                opts.progress.inc(1);
                opts.progress.set_message(raw);
                let target = frankweiler_etl::ids::normalize_id_token(raw);
                fetch_single(
                    &mut client,
                    &db,
                    &orgs,
                    &target,
                    &mut summary,
                    &mut blake3_by_file,
                    &now,
                )
                .await?;
            }
            return Ok::<(), anyhow::Error>(());
        }

        // Pass 1: list every org, classify. Collect the per-org fetch
        // plans so we know the total work up front and can set the
        // progress bar's length exactly once — otherwise a length
        // reset per org makes the bar jump backwards (e.g. `77/58`
        // when the second org's length is smaller than the count
        // already accumulated from the first).
        //
        // No pre-seed: we only ever write a row after a successful
        // detail fetch. The next sync's listing is the source of truth
        // for "what should exist." A previously-failed fetch is
        // naturally retried because no row exists yet.
        struct OrgPlan<'a> {
            org_uuid: String,
            org_name: String,
            ordered: Vec<&'a Value>,
        }
        let mut plans: Vec<OrgPlan> = Vec::new();
        let mut listings_by_org: Vec<(String, String, Vec<Value>)> = Vec::new();
        for org in &orgs {
            let Some(org_uuid) = org.get("uuid").and_then(|v| v.as_str()) else {
                continue;
            };
            let org_name = org
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(&org_uuid[..org_uuid.len().min(8)])
                .to_string();
            let listing = match client
                .list_conversations(org_uuid)
                .instrument(info_span!("anthropic_org_listing", org = %org_name))
                .await
            {
                Ok(l) => l,
                Err(ClaudeError::Forbidden(_)) => {
                    info!(
                        event = "anthropic_org_forbidden",
                        org = %org_name,
                        note = "no chat permission for this org"
                    );
                    summary.forbidden_orgs += 1;
                    continue;
                }
                Err(e) => return Err(anyhow::anyhow!("list conversations for {org_name}: {e}")),
            };
            info!(
                event = "anthropic_org_listing_count",
                org = %org_name,
                count = listing.len()
            );
            sleep(SLEEP_BETWEEN).await;
            listings_by_org.push((org_uuid.to_string(), org_name, listing));
        }

        for (org_uuid, org_name, listing) in &listings_by_org {
            let mut missing: Vec<&Value> = Vec::new();
            let mut stale: Vec<&Value> = Vec::new();
            let mut overlap_force: HashSet<String> = HashSet::new();
            {
                let mut sorted: Vec<&Value> = listing.iter().collect();
                sorted.sort_by(|a, b| {
                    let ka = a.get("updated_at").and_then(|v| v.as_str()).unwrap_or("");
                    let kb = b.get("updated_at").and_then(|v| v.as_str()).unwrap_or("");
                    kb.cmp(ka)
                });
                for c in sorted.iter().take(opts.overlap) {
                    if let Some(u) = c.get("uuid").and_then(|v| v.as_str()) {
                        overlap_force.insert(u.into());
                    }
                }
            }
            let listed_ids: Vec<&str> = listing
                .iter()
                .filter_map(|c| c.get("uuid").and_then(|v| v.as_str()))
                .collect();
            let existing = db.existing_updated_at(&listed_ids).await?;
            let mut up_to_date: usize = 0;
            for item in listing {
                let Some(uuid) = item.get("uuid").and_then(|v| v.as_str()) else {
                    continue;
                };
                let api_updated = item
                    .get("updated_at")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match existing.get(uuid) {
                    Some(stored) if !overlap_force.contains(uuid) => {
                        if stored.as_str() == api_updated {
                            up_to_date += 1;
                        } else {
                            stale.push(item);
                        }
                    }
                    Some(_) => stale.push(item),
                    None => missing.push(item),
                }
            }
            info!(
                event = "anthropic_priority_split",
                org = %org_name,
                missing = missing.len(),
                stale = stale.len(),
                up_to_date = up_to_date,
            );
            summary.skipped += up_to_date;

            let ordered: Vec<&Value> = missing.into_iter().chain(stale).collect();
            plans.push(OrgPlan {
                org_uuid: org_uuid.clone(),
                org_name: org_name.clone(),
                ordered,
            });
        }

        // Pass 2: fetch. The outer bar's length is the sum across all
        // orgs and advances once per chat — so a quick glance answers
        // "how close is the whole sync to done?". Each org also gets
        // its own inner bar (mirroring the per-channel pattern in
        // slack) so the current-org context stays visible.
        let total: usize = plans.iter().map(|p| p.ordered.len()).sum();
        opts.progress.set_length(Some(total as u64));
        for plan in &plans {
            let inner = opts
                .progress
                .child(&format!("claude org: {}", plan.org_name));
            inner.set_length(Some(plan.ordered.len() as u64));
            for item in &plan.ordered {
                let Some(uuid) = item.get("uuid").and_then(|v| v.as_str()) else {
                    continue;
                };
                inner.inc(1);
                inner.set_message(uuid);
                opts.progress.inc(1);
                opts.progress
                    .set_message(&format!("{} {uuid}", plan.org_name));
                match get_conversation_with_403_retry(&mut client, &plan.org_uuid, uuid).await {
                    Ok(outcome) => {
                        summary.forbidden_retry_attempts += outcome.retries as u64;
                        if outcome.retries > 0 {
                            summary.forbidden_retry_recoveries += 1;
                        }
                        save_conversation(
                            &db,
                            &plan.org_uuid,
                            &plan.org_name,
                            uuid,
                            &outcome.value,
                            &now,
                        )
                        .await?;
                        summary.fetched += 1;
                        fetch_files_for(
                            &db,
                            &outcome.value,
                            uuid,
                            &mut summary,
                            &mut blake3_by_file,
                            &now,
                        )
                        .await;
                        if opts.sleep_between > Duration::ZERO {
                            sleep(opts.sleep_between).await;
                        }
                    }
                    Err((e, retries)) => {
                        summary.forbidden_retry_attempts += retries as u64;
                        warn!(event = "anthropic_fetch_error", uuid = uuid, error = %e);
                        let _ = db.record_conversation_error(uuid, &e.to_string()).await;
                        summary.errors += 1;
                    }
                }
            }
            inner.finish_and_clear();
        }
        Ok(())
    };

    let result = work.await;
    summary.total = summary.fetched + summary.skipped;
    summary.requests = client.requests;
    summary.network_seconds = client.network_seconds;
    run.finish(&result, &summary).await;
    result?;
    Ok(summary)
}

#[allow(clippy::too_many_arguments)]
async fn fetch_single(
    client: &mut ClaudeClient,
    db: &RawDb,
    orgs: &[Value],
    conv_uuid: &str,
    summary: &mut FetchSummary,
    blake3_by_file: &mut HashMap<String, String>,
    now: &str,
) -> Result<()> {
    for org in orgs {
        let Some(org_uuid) = org.get("uuid").and_then(|v| v.as_str()) else {
            continue;
        };
        let org_name = org
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or(&org_uuid[..org_uuid.len().min(8)])
            .to_string();
        match client.get_conversation(org_uuid, conv_uuid).await {
            Ok(full) => {
                save_conversation(db, org_uuid, &org_name, conv_uuid, &full, now).await?;
                summary.fetched += 1;
                info!(
                    event = "anthropic_fetch_single_ok",
                    uuid = conv_uuid,
                    org = %org_name
                );
                fetch_files_for(db, &full, conv_uuid, summary, blake3_by_file, now).await;
                return Ok(());
            }
            Err(ClaudeError::Forbidden(_)) => {
                info!(
                    event = "anthropic_fetch_single_forbidden",
                    uuid = conv_uuid,
                    org = %org_name
                );
                continue;
            }
            Err(ClaudeError::Permanent(msg)) if msg.contains("HTTP 404") => {
                info!(
                    event = "anthropic_fetch_single_not_in_org",
                    uuid = conv_uuid,
                    org = %org_name
                );
                continue;
            }
            Err(e) => {
                warn!(event = "anthropic_fetch_error", uuid = conv_uuid, error = %e);
                let _ = db
                    .record_conversation_error(conv_uuid, &e.to_string())
                    .await;
                summary.errors += 1;
                return Err(anyhow::anyhow!("fetch {conv_uuid}: {e}"));
            }
        }
    }
    Err(anyhow::anyhow!(
        "conversation {conv_uuid} not found in any of {} org(s)",
        orgs.len()
    ))
}

/// Backoff delays for transient-403 retries on a single
/// `get_conversation`. claude.ai occasionally returns 403 on detail GETs
/// when listing+detail are issued in rapid succession; the same UUID
/// re-fetched a moment later typically returns 200. Verified by direct
/// probe: a UUID that 403'd inside a run returned 200 to a fresh
/// `latchkey curl` immediately after. We treat Forbidden as transient
/// here (not at the transport layer) so a real org-level permission
/// denial — caught earlier by `list_conversations` — still short-circuits
/// to `anthropic_org_forbidden`.
const FORBIDDEN_RETRY_BACKOFFS: &[Duration] = &[Duration::from_millis(500), Duration::from_secs(2)];

/// Outcome of a 403-retrying detail fetch. `retries` counts the
/// *additional* attempts after the first (so 0 = first try succeeded).
struct RetryOutcome {
    value: Value,
    retries: u32,
}

async fn get_conversation_with_403_retry(
    client: &mut ClaudeClient,
    org_uuid: &str,
    conv_uuid: &str,
) -> Result<RetryOutcome, (ClaudeError, u32)> {
    let mut last_err: Option<ClaudeError> = None;
    for (attempt, delay) in std::iter::once(None)
        .chain(FORBIDDEN_RETRY_BACKOFFS.iter().copied().map(Some))
        .enumerate()
    {
        if let Some(d) = delay {
            sleep(d).await;
        }
        match client.get_conversation(org_uuid, conv_uuid).await {
            Ok(v) => {
                if attempt > 0 {
                    info!(
                        event = "anthropic_fetch_403_retry_ok",
                        uuid = conv_uuid,
                        attempt = attempt,
                    );
                }
                return Ok(RetryOutcome {
                    value: v,
                    retries: attempt as u32,
                });
            }
            Err(ClaudeError::Forbidden(msg)) => {
                warn!(
                    event = "anthropic_fetch_403_transient",
                    uuid = conv_uuid,
                    attempt = attempt,
                    error = %msg,
                );
                last_err = Some(ClaudeError::Forbidden(msg));
            }
            Err(other) => {
                return Err((other, attempt as u32));
            }
        }
    }
    Err((
        last_err.expect("at least one attempt"),
        FORBIDDEN_RETRY_BACKOFFS.len() as u32,
    ))
}

async fn save_conversation(
    db: &RawDb,
    org_uuid: &str,
    org_name: &str,
    uuid: &str,
    full: &Value,
    now: &str,
) -> Result<()> {
    let payload = serde_json::to_string(full).context("serialize conversation")?;
    let name = full.get("name").and_then(|v| v.as_str()).map(String::from);
    let updated_at = full
        .get("updated_at")
        .and_then(|v| v.as_str())
        .map(String::from);
    let row = ConversationRowSchema {
        id_and_payload: WirePayload {
            id: uuid.to_string(),
            payload,
        },
        org_uuid: Some(org_uuid.to_string()),
        org_name: Some(org_name.to_string()),
        name,
        updated_at,
    };
    let mut tx = db
        .pool()
        .begin()
        .await
        .context("begin save_conversation tx")?;
    bulk_upsert_in_tx(&mut tx, &[row], now).await?;
    tx.commit().await.context("commit save_conversation tx")?;
    Ok(())
}

/// Bulk-upsert helpers — same `now` as the rest of the fetch so the
/// bookkeeping sidecars all share a timestamp.
async fn upsert_users(db: &RawDb, payloads: &[Value], now: &str) -> Result<()> {
    if payloads.is_empty() {
        return Ok(());
    }
    let mut rows: Vec<UserRow> = Vec::with_capacity(payloads.len());
    for payload in payloads {
        let Some(id) = payload.get("uuid").and_then(|v| v.as_str()) else {
            continue;
        };
        let email = payload
            .get("email_address")
            .and_then(|v| v.as_str())
            .map(String::from);
        let full_name = payload
            .get("full_name")
            .and_then(|v| v.as_str())
            .map(String::from);
        let payload_str = serde_json::to_string(payload).context("serialize user")?;
        rows.push(UserRow {
            id_and_payload: WirePayload {
                id: id.to_string(),
                payload: payload_str,
            },
            email,
            full_name,
        });
    }
    if rows.is_empty() {
        return Ok(());
    }
    let mut tx = db.pool().begin().await.context("begin upsert_users tx")?;
    bulk_upsert_in_tx(&mut tx, &rows, now).await?;
    tx.commit().await.context("commit upsert_users tx")?;
    Ok(())
}

async fn upsert_orgs(db: &RawDb, payloads: &[Value], now: &str) -> Result<()> {
    if payloads.is_empty() {
        return Ok(());
    }
    let mut rows: Vec<OrgRow> = Vec::with_capacity(payloads.len());
    for payload in payloads {
        let Some(id) = payload.get("uuid").and_then(|v| v.as_str()) else {
            continue;
        };
        let name = payload
            .get("name")
            .and_then(|v| v.as_str())
            .map(String::from);
        let payload_str = serde_json::to_string(payload).context("serialize org")?;
        rows.push(OrgRow {
            id_and_payload: WirePayload {
                id: id.to_string(),
                payload: payload_str,
            },
            name,
        });
    }
    if rows.is_empty() {
        return Ok(());
    }
    let mut tx = db.pool().begin().await.context("begin upsert_orgs tx")?;
    bulk_upsert_in_tx(&mut tx, &rows, now).await?;
    tx.commit().await.context("commit upsert_orgs tx")?;
    Ok(())
}

/// Pull `users.json` entries from an existing bulk-export directory
/// into the DB. Best-effort: missing file is fine.
async fn ingest_export_users(db: &RawDb, export_dir: &Path, now: &str) -> Result<()> {
    let path = export_dir.join("users.json");
    if !path.exists() {
        return Ok(());
    }
    let txt = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    let v: Value =
        serde_json::from_str(&txt).with_context(|| format!("parse {}", path.display()))?;
    if let Some(arr) = v.as_array() {
        if let Err(e) = upsert_users(db, arr, now).await {
            warn!(event = "anthropic_users_upsert_failed", error = %e);
        }
    }
    Ok(())
}

fn pick_user_fields(acct: &Value) -> Value {
    let mut obj = serde_json::Map::new();
    for key in ["uuid", "email_address", "full_name"] {
        if let Some(v) = acct.get(key) {
            obj.insert(key.into(), v.clone());
        }
    }
    Value::Object(obj)
}

/// Walk a conversation tree's `chat_messages[*].files[]` and
/// queue every unique attachment for the end-of-conversation CAS
/// flush. Skips files we already have bytes for.
async fn fetch_files_for(
    db: &RawDb,
    conv: &Value,
    conv_uuid: &str,
    summary: &mut FetchSummary,
    blake3_by_file: &mut HashMap<String, String>,
    now: &str,
) {
    let messages = match conv.get("chat_messages").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return,
    };
    let mut seen: HashSet<String> = HashSet::new();
    let mut targets: Vec<Value> = Vec::new();
    for msg in messages {
        if let Some(files) = msg.get("files").and_then(|v| v.as_array()) {
            for f in files {
                if let Some(id) = f.get("file_uuid").and_then(|v| v.as_str()) {
                    if seen.insert(id.to_string()) {
                        targets.push(f.clone());
                    }
                }
            }
        }
    }
    let mut attach = frankweiler_etl::blob_cas::CasEdgeAccumulator::new();
    for f in &targets {
        let Some(file_uuid) = f.get("file_uuid").and_then(|v| v.as_str()) else {
            continue;
        };
        if let Some(blake3) = blake3_by_file.get(file_uuid) {
            attach.add_known(conv_uuid, file_uuid, blake3.clone());
            summary.skipped_blobs += 1;
            continue;
        }
        let name = f
            .get("file_name")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        match download_one_file(f).await {
            Ok(Some((bytes, content_type))) => {
                let blake3 = frankweiler_etl::blob_cas::blake3_hex(&bytes);
                blake3_by_file.insert(file_uuid.to_string(), blake3);
                attach.add_fetched(conv_uuid, file_uuid, bytes, content_type, name);
                summary.new_blobs += 1;
            }
            Ok(None) => {
                attach.add_failed(conv_uuid, file_uuid, "no bytes");
                summary.failed_blobs += 1;
            }
            Err(e) => {
                warn!(event = "anthropic_media_unexpected_err", file_uuid = %file_uuid, error = %e);
                attach.add_failed(conv_uuid, file_uuid, e.to_string());
                summary.failed_blobs += 1;
            }
        }
    }
    let flush_result = attach
        .flush(db.pool(), db.cas(), |conv_uuid, file_uuid, blake3| {
            ConversationAttachmentRow {
                id: ConversationAttachmentRow::pk_recipe(conv_uuid, file_uuid),
                conversation_uuid: conv_uuid.to_string(),
                file_uuid: file_uuid.to_string(),
                blake3: blake3.map(String::from),
            }
        })
        .await;
    if let Err(e) = flush_result {
        warn!(event = "anthropic_attachment_flush_err", conv = %conv_uuid, error = %e);
    }
    let _ = now;
}

async fn download_one_file(file_obj: &Value) -> Result<Option<(Vec<u8>, Option<String>)>> {
    let Some(file_uuid) = file_obj.get("file_uuid").and_then(|v| v.as_str()) else {
        return Ok(None);
    };
    let preview_path = file_obj
        .get("preview_url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            file_obj
                .get("document_asset")
                .and_then(|d| d.get("url"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        });
    let preview_path = match preview_path {
        Some(p) => p,
        None => {
            warn!(
                event = "anthropic_media_no_preview_url",
                file_uuid = file_uuid
            );
            return Ok(None);
        }
    };
    let url = if preview_path.starts_with("http") {
        preview_path.to_string()
    } else {
        format!("{CLAUDE_ORIGIN}{preview_path}")
    };
    let mime = file_obj
        .get("file_kind")
        .and_then(|v| v.as_str())
        .or_else(|| file_obj.get("mime_type").and_then(|v| v.as_str()));

    let req = HttpRequest::get("anthropic", &url).timeout(ATTACH_FILE_TIMEOUT);
    match latchkey_curl(&req).await {
        Ok(resp) if (200..300).contains(&resp.status) => {
            let header_mime = resp.header("content-type").map(String::from);
            let effective_mime = header_mime.as_deref().or(mime);
            Ok(Some((resp.body, effective_mime.map(String::from))))
        }
        Ok(resp) => {
            let msg = format!("HTTP {}", resp.status);
            warn!(
                event = "anthropic_media_failed",
                file_uuid = file_uuid,
                error = %msg,
            );
            Ok(None)
        }
        Err(e) => {
            let msg = e.to_string();
            warn!(
                event = "anthropic_media_failed",
                file_uuid = file_uuid,
                error = %msg,
            );
            Ok(None)
        }
    }
}
