//! ChatGPT downloader entry point. Port of `src/download/chatgpt_web.py`.
//!
//! Writes into a single doltlite database file
//! (`<data_root>/raw/<name>.doltlite_db`) — one row per `/me` response,
//! per conversation, and per attached file. See `db.rs` for the schema
//! and `frankweiler_etl::doltlite_raw` for the design rationale.
//!
//! The `_fetched_at` synthetic key that the Python downloader stamped
//! into per-conversation JSON files has been promoted to a real
//! bookkeeping column (`conversations_bookkeeping.fetched_at`) — the
//! stored payload is now the raw upstream response byte-for-byte.
//!
//! Auth + Cloudflare clearance is still delegated to `latchkey curl`
//! with `LATCHKEY_CURL=/path/to/curl_impersonate-chrome`.

pub mod api;
pub mod db;
pub mod schema_raw;

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use frankweiler_etl::blob_cas::BlobBundle;
use frankweiler_etl::bulk::bulk_upsert_in_tx;
use frankweiler_etl::doltlite_raw::WirePayload;
use frankweiler_etl::extract_run::ExtractRun;
use frankweiler_etl::latchkey::latchkey_tokio_command;
use frankweiler_time::IsoOffsetTimestamp;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::time::sleep;
use tracing::{info, info_span, instrument, warn, Instrument};

pub use api::{ChatGPTClient, ChatGPTError};
pub use db::{db_path_for, LoadedConversation, LoadedRaw, RawDb};
use schema_raw::{
    attachment_id_recipe, ConversationAttachmentRow, ConversationRow as ConversationRowSchema,
    MeRow,
};

/// Inter-fetch sleep. ChatGPT doesn't appear to throttle us at any
/// polite rate; 100ms keeps us from looking like a tight loop without
/// doubling per-conv latency on top of ~400ms GETs.
pub const SLEEP_BETWEEN: Duration = Duration::from_millis(100);
pub const PAGE_SIZE: usize = 100;

/// File-timeout for attachment GETs through the latchkey shim.
const ATTACH_FILE_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Default)]
pub struct FetchOptions {
    /// Path to the doltlite database file. If the caller passes a
    /// legacy directory, it's rewritten to `<dir>.doltlite_db`.
    /// Ignored for opening when `db` is `Some`.
    pub db_path: PathBuf,
    /// Pre-opened raw DB. When `Some`, `fetch` uses this directly
    /// instead of opening from `db_path`. See the matching field on
    /// the other providers' FetchOptions for rationale.
    pub db: Option<RawDb>,
    pub max_pages: Option<usize>,
    pub limit: Option<usize>,
    pub sleep_between: Duration,
    /// When non-empty, fetch only these conversation ids. Skips the
    /// paginated listing walk; `/me` is still fetched (cheap, captures
    /// account id).
    pub conv_uuids: Vec<String>,
    /// Override the `fetched_at` stamp recorded on each conversation.
    /// When `None`, uses `Local::now()`. The sync orchestrator passes
    /// its `--now` value here so deterministic builds get a stable
    /// stamp.
    pub fetched_at: Option<String>,
    pub progress: frankweiler_etl::progress::Progress,
    /// Cross-provider knobs (`--reset-and-redownload`, etc).
    pub control: frankweiler_etl::control::ExtractControl,
}

#[derive(Debug, Default, Serialize)]
pub struct FetchSummary {
    pub fetched: usize,
    pub skipped: usize,
    pub errors: usize,
    pub listing: usize,
    pub new_blobs: usize,
    pub skipped_blobs: usize,
    pub failed_blobs: usize,
    pub requests: u64,
    pub network_seconds: f64,
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
        tracing::info!(event = "chatgpt_reset_and_redownload");
        db.reset().await.context("reset raw db before redownload")?;
    }
    if opts.control.refetch_blobs {
        tracing::info!(event = "chatgpt_refetch_blobs");
        db.clear_blob_hashes()
            .await
            .context("clear chatgpt_attachments.blake3 before refetch")?;
    }

    let run_config = json!({
        "max_pages": opts.max_pages,
        "limit": opts.limit,
        "conv_uuids": opts.conv_uuids,
    });
    let run = ExtractRun::start(db.pool(), &run_config).await?;

    // One `now` per fetch — threaded into every bulk upsert so all
    // `<table>_bookkeeping.fetched_at` stamps from a single sync share
    // a timestamp. The sync orchestrator passes its `--now` here so
    // deterministic builds get a stable stamp.
    let now = opts
        .fetched_at
        .clone()
        .unwrap_or_else(|| IsoOffsetTimestamp::now_local().to_rfc3339());

    let mut client = ChatGPTClient::new();
    let mut summary = FetchSummary::default();

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
        );

        if !opts.conv_uuids.is_empty() {
            opts.progress.set_length(Some(opts.conv_uuids.len() as u64));
            for raw in &opts.conv_uuids {
                opts.progress.inc(1);
                opts.progress.set_message(raw);
                let target = frankweiler_etl::ids::normalize_id_token(raw);
                match client.get_conversation(&target).await {
                    Ok(full) => {
                        let (title, update_time) = title_and_update_time(&full);
                        let payload =
                            serde_json::to_string(&full).context("serialize conversation")?;
                        upsert_conversations(
                            &db,
                            &[ConversationUpsert {
                                id: target.clone(),
                                title,
                                update_time,
                                payload,
                            }],
                            &now,
                        )
                        .await?;
                        summary.fetched += 1;
                        fetch_attachments_for(&mut client, &db, &full, &mut summary, &now).await;
                        info!(event = "chatgpt_fetch_single_ok", raw = raw, id = %target);
                    }
                    Err(e) => {
                        warn!(event = "chatgpt_fetch_error", raw = raw, id = %target, error = %e);
                        return Err(anyhow::anyhow!("fetch {raw}: {e}"));
                    }
                }
            }
            return Ok::<(), anyhow::Error>(());
        }

        opts.progress.set_message("listing conversations");
        let listing = list_all_conversations(&mut client, opts.max_pages, &opts.progress)
            .instrument(info_span!("chatgpt_list"))
            .await?;
        info!(event = "chatgpt_listing", convs = listing.len());
        summary.listing = listing.len();

        // Skip-check: bulk-read existing `(id, update_time)` for every
        // listed id, then compare to the listing's update_time. Rows
        // we don't have at all → missing. Rows whose stored update_time
        // differs from the listing's → stale. Both fall into the work
        // queue; everything else is up-to-date and skipped.
        //
        // No pre-seed: we only ever write a row after a successful
        // detail fetch. The next sync's listing is the source of truth
        // for "what should exist." A previously-failed fetch is
        // naturally retried because no row exists yet.
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
            // JSON-encode the listing's update_time so its byte shape
            // matches the stored value (the listing can emit a number,
            // string, or null depending on the conversation).
            let api_ut_str = item
                .get("update_time")
                .map(|v| serde_json::to_string(v).unwrap_or_default());
            match (existing.get(cid), api_ut_str.as_deref()) {
                (Some(stored), Some(api)) if stored == api => up_to_date += 1,
                (Some(_), _) => stale.push(item),
                (None, _) => missing.push(item),
            }
        }
        info!(
            event = "chatgpt_priority_split",
            missing = missing.len(),
            stale = stale.len(),
            up_to_date = up_to_date,
        );
        summary.skipped += up_to_date;

        let ordered: Vec<&Value> = missing.into_iter().chain(stale).collect();
        opts.progress.set_length(Some(ordered.len() as u64));
        for item in ordered {
            opts.progress.inc(1);
            if let Some(limit) = opts.limit {
                if summary.fetched + summary.errors >= limit {
                    info!(event = "chatgpt_limit_reached", limit = limit);
                    break;
                }
            }
            let Some(cid) = item.get("id").and_then(|v| v.as_str()) else {
                continue;
            };
            opts.progress.set_message(cid);
            match client.get_conversation(cid).await {
                Ok(full) => {
                    let (title, update_time) = title_and_update_time(&full);
                    let payload = match serde_json::to_string(&full) {
                        Ok(s) => s,
                        Err(e) => {
                            warn!(event = "chatgpt_serialize_error", cid = cid, error = %e);
                            summary.errors += 1;
                            continue;
                        }
                    };
                    upsert_conversations(
                        &db,
                        &[ConversationUpsert {
                            id: cid.to_string(),
                            title,
                            update_time,
                            payload,
                        }],
                        &now,
                    )
                    .await?;
                    summary.fetched += 1;
                    fetch_attachments_for(&mut client, &db, &full, &mut summary, &now).await;
                    if opts.sleep_between > Duration::ZERO {
                        sleep(opts.sleep_between).await;
                    }
                }
                Err(ChatGPTError::RateLimited { path, waited_secs }) => {
                    warn!(
                        event = "chatgpt_rate_limit_giveup",
                        path = %path,
                        waited_secs = waited_secs,
                        fetched = summary.fetched,
                    );
                    break;
                }
                Err(ChatGPTError::Permanent(msg)) => {
                    warn!(event = "chatgpt_fetch_error", cid = cid, error = %msg);
                    let _ = db.record_conversation_error(cid, &msg).await;
                    summary.errors += 1;
                }
            }
        }
        Ok(())
    };

    let result = work.await;
    summary.requests = client.requests;
    summary.network_seconds = client.network_seconds;
    run.finish(&result, &summary).await;
    result?;
    Ok(summary)
}

fn title_and_update_time(full: &Value) -> (Option<String>, Option<String>) {
    let title = full.get("title").and_then(|v| v.as_str()).map(String::from);
    // `update_time` in the detail response is a Unix-epoch float; the
    // listing endpoint returns the same value as a number. We store
    // its JSON string form so the column stays comparable across both
    // shapes ("1737.5" vs "1737.5").
    let update_time = full
        .get("update_time")
        .map(|v| serde_json::to_string(v).unwrap_or_default());
    (title, update_time)
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

/// Build a `MeRow` and bulk-upsert it. Same `now` everywhere so the
/// `me_bookkeeping.fetched_at` stamp matches the rest of the fetch.
async fn upsert_me(db: &RawDb, payload: &Value, now: &str) -> Result<()> {
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
async fn upsert_conversations(db: &RawDb, rows: &[ConversationUpsert], now: &str) -> Result<()> {
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

/// Walk a conversation tree and pull every attachment + asset-pointer
/// blob into the DB. Per the design doc we skip when we already have
/// bytes (signed URLs rotate; bytes don't). Failures bump
/// `attempt_count` and record `last_error`; they don't fail the sync.
///
/// Pending state accumulates in a [`BlobBundle`] — successful fetches
/// go through `bundle.add(...)`, failures through `bundle.add_error(...)`.
/// One flush at end-of-conversation drains the bundle into the CAS
/// (via `BlobCas::put_many`) + the per-provider `chatgpt_attachments`
/// edge table (via `bulk_upsert_in_tx`) + the bookkeeping sidecar
/// (`record_object_error`).
async fn fetch_attachments_for(
    client: &mut ChatGPTClient,
    db: &RawDb,
    conv: &Value,
    summary: &mut FetchSummary,
    now: &str,
) {
    let Some(cid) = conv
        .get("conversation_id")
        .or_else(|| conv.get("id"))
        .and_then(|v| v.as_str())
    else {
        return;
    };
    let mapping = match conv.get("mapping").and_then(|v| v.as_object()) {
        Some(m) => m,
        None => return,
    };
    // Dedupe by file_id within a conversation: identical assets often
    // appear under multiple parts (asset_pointer + attachments mirror).
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
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
            for p in parts {
                let Some(obj) = p.as_object() else { continue };
                if obj.get("content_type").and_then(|v| v.as_str()) != Some("image_asset_pointer") {
                    continue;
                }
                let Some(ptr) = obj.get("asset_pointer").and_then(|v| v.as_str()) else {
                    continue;
                };
                let id = ptr
                    .strip_prefix("sediment://")
                    .or_else(|| ptr.strip_prefix("file-service://"))
                    .unwrap_or(ptr)
                    .to_string();
                if seen.insert(id.clone()) {
                    targets.push((id, None, Some("image/*".into())));
                }
            }
        }
    }
    let mut bundle = BlobBundle::new();
    // failures: ref_id → which we couldn't get bytes for; tracked here
    // so the end-of-walk flush can insert blake3=NULL edge rows for
    // them too (the bundle proper only carries successful fetches).
    let mut failed: Vec<String> = Vec::new();
    for (file_id, name, mime) in targets {
        if db.attachment_has_bytes(&file_id).await.unwrap_or(false) {
            summary.skipped_blobs += 1;
            continue;
        }
        match download_one_file(client, &file_id, name.as_deref(), mime.as_deref()).await {
            Ok(Some((bytes, content_type))) => {
                bundle.add(&file_id, bytes, content_type, name.clone());
                summary.new_blobs += 1;
            }
            Ok(None) => {
                bundle.add_error(&file_id, "no bytes");
                failed.push(file_id.clone());
                summary.failed_blobs += 1;
            }
            Err(e) => {
                warn!(event = "chatgpt_media_unexpected_err", file_id = %file_id, error = %e);
                bundle.add_error(&file_id, e.to_string());
                failed.push(file_id.clone());
                summary.failed_blobs += 1;
            }
        }
    }

    if let Err(e) = flush_attachment_bundle(db, &bundle, &failed, cid, now).await {
        warn!(event = "chatgpt_attachment_flush_err", conv = %cid, error = %e);
    }
}

/// End-of-conversation flush. CAS-pool tx (`put_many`) → entity-pool tx
/// (chunked multi-row UPSERT for both successful fetches and the
/// blake3=NULL stubs for failed ones, plus `record_object_error` per
/// failure into the bookkeeping sidecar). Order: CAS first so the
/// entity-row's `blake3` points at bytes definitely in the CAS.
async fn flush_attachment_bundle(
    db: &RawDb,
    bundle: &BlobBundle,
    failed_refs: &[String],
    conversation_id: &str,
    now: &str,
) -> Result<()> {
    if bundle.is_empty() && bundle.errors().is_empty() {
        return Ok(());
    }
    let cas_inserts = bundle.cas_inserts();
    if !cas_inserts.is_empty() {
        db.cas()
            .put_many(&cas_inserts)
            .await
            .context("chatgpt CAS put_many")?;
    }
    // Build edge rows: one per successful fetch (with blake3) + one
    // per failed fetch (blake3 NULL). Both go through the same bulk
    // upsert so the sidecar's attempt_count moves uniformly.
    let mut rows: Vec<ConversationAttachmentRow> = bundle
        .fetched_refs()
        .map(|f| ConversationAttachmentRow {
            id: attachment_id_recipe(conversation_id, f.ref_id),
            conversation_id: conversation_id.to_string(),
            file_id: f.ref_id.to_string(),
            blake3: Some(f.blake3.to_string()),
        })
        .collect();
    for ref_id in failed_refs {
        rows.push(ConversationAttachmentRow {
            id: attachment_id_recipe(conversation_id, ref_id),
            conversation_id: conversation_id.to_string(),
            file_id: ref_id.clone(),
            blake3: None,
        });
    }
    let mut tx = db
        .pool()
        .begin()
        .await
        .context("begin flush_attachment_bundle tx")?;
    bulk_upsert_in_tx(&mut tx, &rows, now).await?;
    for (ref_id, err) in bundle.errors() {
        let id = attachment_id_recipe(conversation_id, ref_id);
        frankweiler_etl::doltlite_raw::record_object_error(
            &mut tx,
            "chatgpt_attachments",
            &id,
            err,
        )
        .await?;
    }
    tx.commit()
        .await
        .context("commit flush_attachment_bundle tx")?;
    Ok(())
}

/// Fetch one attachment's bytes via the two-hop dance: metadata via
/// latchkey (auth attached), then `latchkey curl -fSL` on the signed
/// URL (no auth — Azure rejects the chatgpt cookie). On success returns
/// `(bytes, content_type)` for the caller to feed into the
/// per-conversation [`BlobBundle`]. `Ok(None)` means "no bytes for
/// this file" — caller logs a stub edge row + bookkeeping error.
async fn download_one_file(
    client: &mut ChatGPTClient,
    file_id: &str,
    name: Option<&str>,
    mime: Option<&str>,
) -> Result<Option<(Vec<u8>, Option<String>)>> {
    let _ = name; // upstream_name is no longer stored on the edge row
                  // Step 1: metadata fetch.
    let meta = match client
        .get(&format!("/backend-api/files/{file_id}/download"))
        .await
    {
        Ok(v) => v,
        Err(e) => {
            warn!(
                event = "chatgpt_media_meta_failed",
                file_id = file_id,
                error = %e,
            );
            return Ok(None);
        }
    };
    let signed = match meta.get("download_url").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            warn!(event = "chatgpt_media_no_download_url", file_id = file_id);
            return Ok(None);
        }
    };

    // Step 2: signed-URL GET via latchkey shim. We write to a tempfile
    // and slurp the bytes — keeps the existing curl shellout shape
    // (which uses `-o <path>`) and side-steps any binary-stdio
    // weirdness. The tempfile is deleted automatically.
    let tmp = tempfile::NamedTempFile::new().context("create blob tempfile")?;
    let mut cmd = latchkey_tokio_command();
    cmd.arg("curl")
        .arg("-fSL")
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
        warn!(
            event = "chatgpt_media_failed",
            file_id = file_id,
            exit = proc.status.code().unwrap_or(-1),
            stderr = %tail.trim(),
        );
        return Ok(None);
    }
    let bytes =
        std::fs::read(tmp.path()).with_context(|| format!("read tempfile for {file_id}"))?;
    Ok(Some((bytes, mime.map(String::from))))
}

#[instrument(skip(client))]
async fn list_all_conversations(
    client: &mut ChatGPTClient,
    max_pages: Option<usize>,
    progress: &frankweiler_etl::progress::Progress,
) -> Result<Vec<Value>> {
    let mut items: Vec<Value> = Vec::new();
    let mut offset = 0usize;
    let mut pages = 0usize;
    loop {
        let page = client
            .list_conversations_page(offset, PAGE_SIZE)
            .await
            .map_err(|e| anyhow::anyhow!("list page offset={offset}: {e}"))?;
        let page_items: Vec<Value> = page
            .get("items")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        let total = page.get("total").and_then(|v| v.as_u64());
        info!(
            event = "chatgpt_listing_page",
            offset = offset,
            got = page_items.len(),
            total = total.unwrap_or(0),
            cum = items.len() + page_items.len(),
        );
        let got = page_items.len();
        items.extend(page_items);
        offset += got;
        pages += 1;
        progress.set_message(&format!("listing page {pages}, {} convs", items.len()));
        if got == 0 {
            break;
        }
        if let Some(t) = total {
            if offset as u64 >= t {
                break;
            }
        }
        if let Some(cap) = max_pages {
            if pages >= cap {
                info!(event = "chatgpt_listing_capped", max_pages = cap);
                break;
            }
        }
        sleep(SLEEP_BETWEEN).await;
    }
    Ok(items)
}
