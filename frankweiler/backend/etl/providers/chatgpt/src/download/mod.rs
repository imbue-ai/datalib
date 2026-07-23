//! ChatGPT downloader entry point. Port of `src/download/chatgpt_web.py`.
//!
//! Writes into a single doltlite database file
//! (`<data_root>/<name>/raw/entities.doltlite_db`) — one row per `/me` response,
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
use chrono::DateTime;
use frankweiler_etl::bulk::bulk_upsert_in_tx;
use frankweiler_etl::doltlite_raw::WirePayload;
use frankweiler_etl::download_run::DownloadRun;
use frankweiler_etl::http::IMPERSONATE_MARKER_HEADER;
use frankweiler_etl::latchkey::latchkey_tokio_command;
use frankweiler_time::IsoOffsetTimestamp;
use serde::Serialize;
use serde_json::{json, Value};
use tokio::time::sleep;
use tracing::{info, info_span, instrument, warn, Instrument};

pub use api::{ChatGPTClient, ChatGPTError};
pub use db::{db_path_for, LoadedConversation, LoadedRaw, RawDb};
use frankweiler_etl::blob_cas::CasEdgeRow as _;
use schema_raw::{ConversationAttachmentRow, ConversationRow as ConversationRowSchema, MeRow};

/// Inter-fetch sleep. ChatGPT doesn't appear to throttle us at any
/// polite rate; 100ms keeps us from looking like a tight loop without
/// doubling per-conv latency on top of ~400ms GETs.
pub const SLEEP_BETWEEN: Duration = Duration::from_millis(100);
pub const PAGE_SIZE: usize = 100;

/// File-timeout for attachment GETs through the latchkey shim.
const ATTACH_FILE_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(Debug, Clone, Default)]
pub struct FetchOptions {
    /// Path to the doltlite database file. The entity db lives inside
    /// the per-source directory as `entities.doltlite_db` (the dir is
    /// created if needed). Ignored for opening when `db` is `Some`.
    pub db_path: PathBuf,
    /// Pre-opened raw DB. When `Some`, `fetch` uses this directly
    /// instead of opening from `db_path`. See the matching field on
    /// the other providers' FetchOptions for rationale.
    pub db: Option<RawDb>,
    pub max_pages: Option<usize>,
    pub limit: Option<usize>,
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
    /// Override the `fetched_at` stamp recorded on each conversation.
    /// When `None`, uses `Local::now()`. The sync orchestrator passes
    /// its `--now` value here so deterministic builds get a stable
    /// stamp.
    pub fetched_at: Option<String>,
    pub progress: frankweiler_etl::progress::Progress,
    /// Cross-provider knobs (`--reset-and-redownload`, etc).
    pub control: frankweiler_etl::control::DownloadControl,
}

#[derive(Debug, Default, Serialize)]
pub struct FetchSummary {
    pub fetched: usize,
    pub skipped: usize,
    /// Listed items ignored because their `update_time` predates the
    /// configured `since`. Items behind an early-stopped listing walk
    /// are never listed at all and are not counted here. Not counted
    /// in `skipped` (which means "in scope and already up to date").
    pub out_of_scope: usize,
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

    // Canonicalized to whole-second epoch, the same grain the
    // skip-check compares `update_time`s at (see `update_time_secs`).
    let since_secs = opts
        .since
        .as_deref()
        .map(parse_since_secs)
        .transpose()
        .with_context(|| format!("sync.since {:?}", opts.since))?;

    let run_config = json!({
        "max_pages": opts.max_pages,
        "limit": opts.limit,
        "since": opts.since,
        "conv_uuids": opts.conv_uuids,
    });
    let run = DownloadRun::start(db.pool(), &run_config).await?;

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

    // Run-scoped `(file_id → blake3)` cache: loaded once up-front so
    // the per-file dedupe check inside `fetch_attachments_for` is a
    // HashMap hit instead of a SQLite round trip. Successful
    // downloads insert into it so files referenced by multiple
    // conversations in the same run hit the cache on every reference
    // after the first.
    let mut blake3_by_file = db.load_attachment_blake3s().await?;

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
                        fetch_attachments_for(
                            &mut client,
                            &db,
                            &full,
                            &mut summary,
                            &mut blake3_by_file,
                            &now,
                        )
                        .await;
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
        let listing =
            list_all_conversations(&mut client, opts.max_pages, since_secs, &opts.progress)
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
            // `since` scope filter: out-of-scope items are never
            // detail-fetched. Items with an unparseable `update_time`
            // fall through in scope (fetch rather than silently drop).
            // The filter only gates fetching — already-stored rows are
            // untouched — so moving `since` further back later
            // backfills the newly-in-scope conversations as missing.
            if let (Some(cutoff), Some(api_secs)) = (
                since_secs,
                item.get("update_time").and_then(update_time_secs),
            ) {
                if api_secs < cutoff {
                    summary.out_of_scope += 1;
                    continue;
                }
            }
            match existing.get(cid) {
                None => missing.push(item),
                // Canonicalize both sides to a whole-second epoch before
                // comparing. The stored value is the *detail* endpoint's
                // Unix-epoch float; `item`'s is the *listing* endpoint's
                // ISO-8601 string — comparing the raw JSON encodings
                // never matches, so every conversation looks stale and
                // gets re-fetched (see `update_time_secs`). Either side
                // failing to canonicalize falls through to `stale`, the
                // safe (re-fetch) direction.
                Some(stored) => {
                    let stored_secs = stored_update_time_secs(stored);
                    let api_secs = item.get("update_time").and_then(update_time_secs);
                    match (stored_secs, api_secs) {
                        (Some(a), Some(b)) if a == b => up_to_date += 1,
                        _ => stale.push(item),
                    }
                }
            }
        }
        info!(
            event = "chatgpt_priority_split",
            missing = missing.len(),
            stale = stale.len(),
            up_to_date = up_to_date,
            out_of_scope = summary.out_of_scope,
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
                    fetch_attachments_for(
                        &mut client,
                        &db,
                        &full,
                        &mut summary,
                        &mut blake3_by_file,
                        &now,
                    )
                    .await;
                    if opts.sleep_between > Duration::ZERO {
                        sleep(opts.sleep_between).await;
                    }
                }
                Err(ChatGPTError::RateLimited { path, reason }) => {
                    warn!(
                        event = "chatgpt_rate_limit_giveup",
                        path = %path,
                        reason = %reason,
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
    // `update_time` in the detail response is a Unix-epoch float, which
    // we store JSON-encoded ("1710959331.420159"). Note the *listing*
    // endpoint reports the same instant as an ISO-8601 string, so the
    // skip-check can't compare the stored text byte-for-byte against the
    // listing value — it canonicalizes both via `update_time_secs`.
    let update_time = full
        .get("update_time")
        .map(|v| serde_json::to_string(v).unwrap_or_default());
    (title, update_time)
}

/// Reduce an `update_time` value to a whole-second Unix epoch for the
/// listing skip-check.
///
/// The two endpoints disagree on shape: the `/conversations` listing
/// returns `update_time` as an ISO-8601 string
/// (`"2024-03-20T18:28:51.420159+00:00"`) while `/conversation/{id}`
/// returns it as a Unix-epoch float (`1710959331.420159`). We store the
/// detail float in `conversations.update_time`, then compare it against
/// the listing string on the next sync — so a raw byte comparison never
/// matches and every conversation looks stale, defeating incremental
/// resume (this regressed the Python-era fix in commit 1fc3ee8).
/// Canonicalizing both sides to whole seconds restores a like-for-like
/// comparison. Sub-second precision is dropped on purpose: a
/// conversation's `update_time` only advances when it gains a message,
/// so seconds suffice to spot real changes and we side-step float/ISO
/// sub-second formatting noise.
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

/// Canonicalize the *stored* column, which SQLite hands back as the
/// JSON-encoded text we wrote (`1710959331.420159` for a float,
/// `"…iso…"` for a string). Re-parse to recover the value's shape, then
/// reduce to seconds via [`update_time_secs`].
fn stored_update_time_secs(json_encoded: &str) -> Option<i64> {
    let v: Value = serde_json::from_str(json_encoded).ok()?;
    update_time_secs(&v)
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
    blake3_by_file: &mut std::collections::HashMap<String, String>,
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
    let mut attach = frankweiler_etl::blob_cas::CasEdgeAccumulator::new();
    for (file_id, name, mime) in targets {
        if let Some(blake3) = blake3_by_file.get(&file_id) {
            attach.add_known(cid, &file_id, blake3.clone());
            summary.skipped_blobs += 1;
            continue;
        }
        match download_one_file(client, &file_id, name.as_deref(), mime.as_deref()).await {
            Ok(Some((bytes, content_type))) => {
                let blake3 = frankweiler_etl::blob_cas::blake3_hex(&bytes);
                blake3_by_file.insert(file_id.clone(), blake3);
                attach.add_fetched(cid, &file_id, bytes, content_type, name.clone());
                summary.new_blobs += 1;
            }
            Ok(None) => {
                attach.add_failed(cid, &file_id, "no bytes");
                summary.failed_blobs += 1;
            }
            Err(e) => {
                warn!(event = "chatgpt_media_unexpected_err", file_id = %file_id, error = %e);
                attach.add_failed(cid, &file_id, e.to_string());
                summary.failed_blobs += 1;
            }
        }
    }

    let flush_result = attach
        .flush(db.pool(), db.cas(), |conv_id, file_id, blake3| {
            ConversationAttachmentRow {
                id: ConversationAttachmentRow::pk_recipe(conv_id, file_id),
                conversation_id: conv_id.to_string(),
                file_id: file_id.to_string(),
                blake3: blake3.map(String::from),
            }
        })
        .await;
    if let Err(e) = flush_result {
        warn!(event = "chatgpt_attachment_flush_err", conv = %cid, error = %e);
    }
    let _ = now;
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
    // The signed CDN URL is CF-fronted; mark the request so the dispatch
    // curl routes it to the impersonating curl.
    cmd.arg("curl")
        .arg("-fSL")
        .arg("-H")
        .arg(IMPERSONATE_MARKER_HEADER)
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
    since_secs: Option<i64>,
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
            break;
        }
        if page_ends_before_since {
            info!(event = "chatgpt_listing_since_stop", pages = pages);
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

/// Parse a `since` config value — full RFC 3339 or bare `YYYY-MM-DD`
/// (assumed UTC midnight) — down to the whole-second Unix epoch the
/// skip-check compares at. Same accepted forms as slack's `since` and
/// anthropic's.
fn parse_since_secs(s: &str) -> Result<i64> {
    let t = frankweiler_time::parse_strict(s)
        .or_else(|_| frankweiler_time::parse_yyyy_mm_dd_assumed_utc(s))
        .with_context(|| format!("expected RFC 3339 or YYYY-MM-DD, got {s:?}"))?;
    Ok(t.inner().timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Derive the ISO-8601 string the *listing* endpoint would report
    /// for a given detail-endpoint epoch float, using the same format
    /// the live API emits (microseconds, explicit `+00:00`).
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

    #[test]
    fn stored_update_time_secs_reparses_json_encoded_text() {
        // The column comes back as the JSON text we wrote. Float and ISO
        // encodings of the same instant must reduce to the same second.
        let epoch = 1_710_959_331.420159_f64;
        let float_text = serde_json::to_string(&json!(epoch)).unwrap();
        let iso_text = serde_json::to_string(&json!(iso_for_epoch(epoch))).unwrap();
        assert_eq!(stored_update_time_secs(&float_text), Some(1_710_959_331));
        assert_eq!(
            stored_update_time_secs(&float_text),
            stored_update_time_secs(&iso_text)
        );
        // Sub-second drift between the two endpoints is collapsed away.
        let jittered = serde_json::to_string(&json!(epoch + 0.4)).unwrap();
        assert_eq!(
            stored_update_time_secs(&jittered),
            stored_update_time_secs(&float_text)
        );
        // Unparseable text is `None` → caller treats the row as stale.
        assert_eq!(stored_update_time_secs("garbage"), None);
        assert_eq!(update_time_secs(&Value::Null), None);
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
        // against the parsed cutoff — same grain as the skip-check, so
        // a listing ISO string on the cutoff second is in scope.
        let cutoff = parse_since_secs("2024-03-20T18:28:51Z").unwrap();
        let on_boundary = json!(iso_for_epoch(1_710_959_331.9));
        let just_before = json!(iso_for_epoch(1_710_959_330.1));
        assert!(update_time_secs(&on_boundary).unwrap() >= cutoff);
        assert!(update_time_secs(&just_before).unwrap() < cutoff);
    }
}
