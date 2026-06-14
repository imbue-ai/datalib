//! Slack API transport: latchkey curl shellout with retry.
//!
//! Both `slack.com/api/` and `files.slack.com/` are covered by the
//! `slack` service's `baseApiUrls` (latchkey ≥ 2.11.2), so a single
//! credential signs both API calls and file downloads.

use std::collections::{BTreeMap, HashMap};
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::time::sleep;
use tracing::{debug, instrument, warn};

use frankweiler_etl::blob_cas::{CasEdgeAccumulator, CasEdgeRow as _};
use frankweiler_etl::events;
use frankweiler_etl::http::{latchkey_curl, HttpError, HttpRequest};
use frankweiler_etl::latchkey::latchkey_tokio_command;

use super::db::RawDb;
use super::schema_raw::{slack_message_uuid, SlackAttachmentRow};

pub const LATCHKEY_TIMEOUT: Duration = Duration::from_secs(60);
pub const LATCHKEY_FILE_TIMEOUT: Duration = Duration::from_secs(600);
pub const RATE_LIMIT_MAX_RETRIES: u32 = 7;
pub const RATE_LIMIT_INITIAL_BACKOFF: Duration = Duration::from_secs(2);
pub const RATE_LIMIT_MAX_BACKOFF: Duration = Duration::from_secs(60);

#[derive(thiserror::Error, Debug)]
pub enum SlackError {
    #[error("rate limited: {0}")]
    RateLimited(String),
    #[error("transient: {0}")]
    Transient(String),
    #[error("{0}")]
    Permanent(String),
}

/// Successful Slack API call plus wall-clock duration of the underlying
/// HTTP exchange.
pub struct SlackCall {
    pub response: Value,
    pub duration_ms: u64,
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

pub(crate) fn build_url(method: &str, params: &BTreeMap<String, String>) -> String {
    let base = format!("https://slack.com/api/{}", method);
    if params.is_empty() {
        return base;
    }
    let qs: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
        .collect();
    format!("{}?{}", base, qs.join("&"))
}

async fn call_slack_once(
    method: &str,
    params: &BTreeMap<String, String>,
) -> Result<Value, SlackError> {
    let url = build_url(method, params);
    let req = HttpRequest::get("slack", &url).timeout(LATCHKEY_TIMEOUT);
    let resp = latchkey_curl(&req).await.map_err(|e: HttpError| match e {
        HttpError::Timeout { .. } => SlackError::Transient(format!("{method}: {e}")),
        HttpError::Curl {
            exit: 7 | 28 | 35 | 56,
            ..
        } => SlackError::Transient(format!("{method}: {e}")),
        HttpError::PlaybackMiss(msg) => SlackError::Permanent(format!("{method}: {msg}")),
        _ => SlackError::Permanent(format!("{method}: {e}")),
    })?;

    if resp.status != 200 {
        return Err(SlackError::Permanent(format!(
            "{method}: HTTP {} body={:?}",
            resp.status,
            resp.body_str().chars().take(200).collect::<String>()
        )));
    }
    let body = resp.body_str();
    let data: Value = serde_json::from_str(&body).map_err(|e| {
        let preview: String = body.chars().take(200).collect();
        SlackError::Permanent(format!("{}: invalid JSON: {:?} ({})", method, preview, e))
    })?;
    let ok = data.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    if !ok {
        let err = data
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        if err == "ratelimited" {
            return Err(SlackError::RateLimited(method.to_string()));
        }
        return Err(SlackError::Permanent(format!(
            "{}: ok=false error={:?}",
            method, err
        )));
    }
    Ok(data)
}

#[instrument(skip(params), fields(method = method))]
pub async fn call_slack(
    method: &str,
    params: &BTreeMap<String, String>,
) -> Result<SlackCall, SlackError> {
    let mut backoff = RATE_LIMIT_INITIAL_BACKOFF;
    for attempt in 0..=RATE_LIMIT_MAX_RETRIES {
        let t0 = std::time::Instant::now();
        match call_slack_once(method, params).await {
            Ok(v) => {
                let duration_ms = t0.elapsed().as_millis() as u64;
                let bytes = v.to_string().len() as u64;
                events::item_fetched(&format!("slack.api/{}", method), bytes, duration_ms);
                return Ok(SlackCall {
                    response: v,
                    duration_ms,
                });
            }
            Err(SlackError::RateLimited(_)) => {
                if attempt == RATE_LIMIT_MAX_RETRIES {
                    return Err(SlackError::Permanent(format!(
                        "{}: rate-limited after {} retries",
                        method, attempt
                    )));
                }
                warn!(
                    event = "slack_rate_limited",
                    method = method,
                    attempt = attempt + 1,
                    max_retries = RATE_LIMIT_MAX_RETRIES,
                    backoff_ms = backoff.as_millis() as u64,
                );
            }
            Err(SlackError::Transient(msg)) => {
                if attempt == RATE_LIMIT_MAX_RETRIES {
                    return Err(SlackError::Permanent(msg));
                }
                warn!(
                    event = "slack_transient_error",
                    method = method,
                    error = %msg,
                    backoff_ms = backoff.as_millis() as u64,
                );
            }
            Err(e @ SlackError::Permanent(_)) => return Err(e),
        }
        sleep(backoff).await;
        backoff = std::cmp::min(backoff * 2, RATE_LIMIT_MAX_BACKOFF);
    }
    Err(SlackError::Permanent(format!(
        "{}: exhausted retries",
        method
    )))
}

// ---------------------------------------------------------------------------
// File-download path: `latchkey curl` against files.slack.com.
// ---------------------------------------------------------------------------

/// End-of-channel flush. Delegates to the shared
/// [`CasEdgeAccumulator::flush`] with a slack-specific row builder.
pub async fn flush_channel_attachments(db: &RawDb, attach: &CasEdgeAccumulator) -> Result<()> {
    attach
        .flush(db.pool(), db.cas(), |message_uuid, file_id, blake3| {
            SlackAttachmentRow {
                id: SlackAttachmentRow::pk_recipe(message_uuid, file_id),
                message_uuid: message_uuid.to_string(),
                file_id: file_id.to_string(),
                blake3: blake3.map(String::from),
            }
        })
        .await
}

/// Walk `messages[].files[]` and download each into the per-channel
/// [`ChannelAttachments`]. `messages` is the raw array from a
/// `conversations.history` or `conversations.replies` response. When
/// `thread_ts` is supplied (replies path), Slack sometimes omits the
/// per-message `thread_ts` field on the parent inline copy; that's
/// fine for attachment edge keys because the message_uuid is derived
/// from `ts` alone, not from the thread bucket.
#[allow(clippy::too_many_arguments)]
pub async fn download_files_for_messages(
    db: &RawDb,
    team_id: &str,
    channel_id: &str,
    messages: &[Value],
    _thread_ts: Option<&str>,
    attach: &mut CasEdgeAccumulator,
    blake3_by_file: &mut HashMap<String, String>,
    blob_size_limit_bytes: Option<u64>,
) -> Result<BTreeMap<String, usize>> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for k in [
        "downloaded",
        "skipped",
        "tombstone",
        "external",
        "error",
        "too_large",
    ] {
        counts.insert(k.to_string(), 0);
    }
    for m in messages {
        let Some(message_ts) = m.get("ts").and_then(|v| v.as_str()) else {
            continue;
        };
        let message_uuid = slack_message_uuid(team_id, channel_id, message_ts);
        let Some(files) = m.get("files").and_then(|f| f.as_array()) else {
            continue;
        };
        for f in files {
            let outcome = download_one_file(
                db,
                &message_uuid,
                f,
                attach,
                blake3_by_file,
                blob_size_limit_bytes,
            )
            .await?;
            *counts.entry(outcome.to_string()).or_insert(0) += 1;
        }
    }
    debug!(
        event = "slack_media_summary",
        downloaded = counts.get("downloaded").copied().unwrap_or(0),
        skipped = counts.get("skipped").copied().unwrap_or(0),
        errors = counts.get("error").copied().unwrap_or(0),
        external = counts.get("external").copied().unwrap_or(0),
    );
    Ok(counts)
}

/// Fetch one file's bytes (or note it as skipped/external/tombstoned)
/// and feed the outcome into the per-channel accumulator. Trust-our-
/// copy: signed URLs rotate, bytes don't, so a `file_id` we've
/// already hashed never re-downloads.
async fn download_one_file(
    _db: &RawDb,
    message_uuid: &str,
    file_obj: &Value,
    attach: &mut CasEdgeAccumulator,
    blake3_by_file: &mut HashMap<String, String>,
    blob_size_limit_bytes: Option<u64>,
) -> Result<&'static str> {
    let file_id = match file_obj.get("id").and_then(|v| v.as_str()) {
        Some(s) => s,
        None => return Ok("tombstone"),
    };
    if file_obj.get("mode").and_then(|v| v.as_str()) == Some("tombstone") {
        return Ok("tombstone");
    }
    if file_obj
        .get("is_external")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return Ok("external");
    }
    let url = file_obj
        .get("url_private_download")
        .and_then(|v| v.as_str())
        .or_else(|| file_obj.get("url_private").and_then(|v| v.as_str()));
    let url = match url {
        Some(u) => u,
        None => return Ok("external"),
    };

    // Skip-check: the run-scoped `(file_id → blake3)` cache was
    // pre-loaded at fetch entry and is updated in-place by every
    // successful download below. A hit means we either already had
    // the bytes from a prior sync or another message in this run
    // landed them; either way, no fetch needed — just record the
    // (message_uuid, file_id) edge with the known hash.
    if let Some(blake3) = blake3_by_file.get(file_id) {
        attach.add_known(message_uuid, file_id, blake3.clone());
        return Ok("skipped");
    }

    if let (Some(limit), Some(size)) = (
        blob_size_limit_bytes,
        file_obj.get("size").and_then(|v| v.as_u64()),
    ) {
        if size > limit {
            debug!(
                event = "slack_media_too_large",
                file_id = file_id,
                size = size,
                limit = limit,
            );
            attach.add_failed(
                message_uuid,
                file_id,
                format!("size {size} > limit {limit}"),
            );
            return Ok("too_large");
        }
    }

    let name = file_obj.get("name").and_then(|v| v.as_str());
    let mime = file_obj.get("mimetype").and_then(|v| v.as_str());

    let tmp = tempfile::NamedTempFile::new().context("create blob tempfile")?;
    let mut cmd = latchkey_tokio_command();
    cmd.arg("curl")
        .arg("-fSL")
        .arg("-o")
        .arg(tmp.path())
        .arg(url);

    let proc = tokio::time::timeout(LATCHKEY_FILE_TIMEOUT, cmd.output())
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
            event = "slack_media_failed",
            file_id = file_id,
            name = name.unwrap_or(""),
            exit = proc.status.code().unwrap_or(-1),
            stderr = %tail.trim(),
        );
        attach.add_failed(message_uuid, file_id, tail.trim().to_string());
        return Ok("error");
    }
    let bytes =
        std::fs::read(tmp.path()).with_context(|| format!("read tempfile for {file_id}"))?;
    let len = bytes.len() as u64;
    // Compute blake3 once, stamp into the run-scoped cache so later
    // messages referencing the same file_id hit the cache, then hand
    // bytes to the bundle.
    let blake3 = frankweiler_etl::blob_cas::blake3_hex(&bytes);
    blake3_by_file.insert(file_id.to_string(), blake3);
    attach.add_fetched(
        message_uuid,
        file_id,
        bytes,
        mime.map(String::from),
        name.map(String::from),
    );
    events::item_fetched(url, len, 0);
    debug!(
        event = "slack_media_downloaded",
        file_id = file_id,
        bytes = len
    );
    Ok("downloaded")
}
