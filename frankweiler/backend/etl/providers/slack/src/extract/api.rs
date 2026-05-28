//! Slack API transport: latchkey curl shellout with retry.
//!
//! Both `slack.com/api/` and `files.slack.com/` are covered by the
//! `slack` service's `baseApiUrls` (latchkey ≥ 2.11.2), so a single
//! credential signs both API calls and file downloads.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::time::sleep;
use tracing::{debug, instrument, warn};

use frankweiler_etl::blobs::safe_filename;
use frankweiler_etl::events;
use frankweiler_etl::http::{latchkey_curl, HttpError, HttpRequest};
use frankweiler_etl::latchkey::latchkey_tokio_command;

use super::db::RawDb;

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
/// HTTP exchange. The caller stamps this into the raw-API capture so
/// stored fixtures double as latency samples.
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
    let resp = latchkey_curl(&req).await.map_err(|e: HttpError| {
        // Curl transport-level errors (DNS/TLS/timeout) are transient
        // for the slack retry loop; everything else stays permanent.
        match e {
            HttpError::Timeout { .. } => SlackError::Transient(format!("{method}: {e}")),
            HttpError::Curl {
                exit: 7 | 28 | 35 | 56,
                ..
            } => SlackError::Transient(format!("{method}: {e}")),
            HttpError::PlaybackMiss(msg) => SlackError::Permanent(format!("{method}: {msg}")),
            _ => SlackError::Permanent(format!("{method}: {e}")),
        }
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
// File-download path: `latchkey curl` against files.slack.com, which
// the `slack` service's baseApiUrls covers (latchkey ≥ 2.11.2).
// ---------------------------------------------------------------------------

/// Download one file's bytes into the doltlite `blobs` table. The
/// `owning_id` we record is the channel containing the message that
/// references the file — Slack files can be shared across channels,
/// but bytes-are-bytes and the first downloader wins (subsequent
/// references no-op via `blob_exists`).
pub async fn download_one_file(
    db: &RawDb,
    channel_id: &str,
    file_obj: &Value,
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

    // Trust-our-copy refetch policy: signed URLs rotate but bytes don't.
    if db.blob_exists(file_id).await.unwrap_or(false) {
        return Ok("skipped");
    }

    let name = safe_filename(file_obj.get("name").and_then(|v| v.as_str()), file_id);
    let mime = file_obj.get("mimetype").and_then(|v| v.as_str());

    // Tempfile + `latchkey curl -o` matches the chatgpt/anthropic pattern.
    // files.slack.com is covered by the `slack` service's baseApiUrls.
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
            name = %name,
            exit = proc.status.code().unwrap_or(-1),
            stderr = %tail.trim(),
        );
        let _ = db
            .record_blob_error(file_id, channel_id, "file", tail.trim())
            .await;
        return Ok("error");
    }
    let bytes =
        std::fs::read(tmp.path()).with_context(|| format!("read tempfile for {file_id}"))?;
    let len = bytes.len() as u64;
    db.upsert_blob_bytes(file_id, "file", channel_id, "file", mime, &bytes, Some(url))
        .await?;
    events::item_fetched(url, len, 0);
    debug!(
        event = "slack_media_downloaded",
        file_id = file_id,
        bytes = len
    );
    Ok("downloaded")
}

/// Walk message records for `files[]` and download each into the
/// doltlite blob store. `messages` is the raw array from a
/// `conversations.history` or `conversations.replies` response.
pub async fn download_files_for_messages(
    db: &RawDb,
    channel_id: &str,
    messages: &[Value],
) -> Result<BTreeMap<String, usize>> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for k in ["downloaded", "skipped", "tombstone", "external", "error"] {
        counts.insert(k.to_string(), 0);
    }
    let mut targets: Vec<Value> = Vec::new();
    for m in messages {
        if let Some(files) = m.get("files").and_then(|f| f.as_array()) {
            for f in files {
                targets.push(f.clone());
            }
        }
    }
    // Pre-seed a blob stub (id + url, no bytes) for every fetchable
    // file before we start downloading. Lets tooling count
    // "known-but-undownloaded" mid-run / after a Ctrl-C; the
    // subsequent upsert_blob_bytes overwrites the stub on success.
    // Skip tombstones and externals (we won't try to fetch those).
    for f in &targets {
        let Some(id) = f.get("id").and_then(|v| v.as_str()) else {
            continue;
        };
        if f.get("mode").and_then(|v| v.as_str()) == Some("tombstone") {
            continue;
        }
        if f.get("is_external")
            .and_then(|v| v.as_bool())
            .unwrap_or(false)
        {
            continue;
        }
        let url = f
            .get("url_private_download")
            .and_then(|v| v.as_str())
            .or_else(|| f.get("url_private").and_then(|v| v.as_str()));
        if url.is_none() {
            continue;
        }
        let mime = f.get("mimetype").and_then(|v| v.as_str());
        let _ = db.pre_seed_blob_stub(id, channel_id, mime, url).await;
    }
    for f in &targets {
        let outcome = download_one_file(db, channel_id, f).await?;
        *counts.entry(outcome.to_string()).or_insert(0) += 1;
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
