//! Slack API transport: latchkey curl shellout with retry.
//!
//! Both `slack.com/api/` and `files.slack.com/` are covered by the
//! `slack` service's `baseApiUrls` (latchkey ≥ 2.11.2), so a single
//! credential signs both API calls and file downloads.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tokio::time::sleep;
use tracing::{debug, instrument, warn};

use frankweiler_etl::blobs::safe_filename;
use frankweiler_etl::http::{latchkey_curl, HttpError, HttpRequest};
use frankweiler_etl::latchkey::latchkey_tokio_command;
use frankweiler_etl::obs::events;

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

pub async fn download_one_file(file_obj: &Value, media_dir: &Path) -> Result<&'static str> {
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

    let name = safe_filename(file_obj.get("name").and_then(|v| v.as_str()), file_id);
    let target_dir = media_dir.join(file_id);
    let target = target_dir.join(&name);
    if let Ok(meta) = std::fs::metadata(&target) {
        if meta.len() > 0 {
            return Ok("skipped");
        }
    }
    std::fs::create_dir_all(&target_dir)?;

    // Goes through latchkey → in-tree curl shim. files.slack.com is
    // in the `slack` service's baseApiUrls, so latchkey injects the
    // same auth it would for slack.com/api/.
    let mut cmd = latchkey_tokio_command();
    cmd.arg("curl").arg("-fSL").arg("-o").arg(&target).arg(url);

    let proc = tokio::time::timeout(LATCHKEY_FILE_TIMEOUT, cmd.output())
        .await
        .context("file curl timed out")?
        .context("file curl spawn failed")?;
    if !proc.status.success() {
        let _ = std::fs::remove_file(&target);
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
        return Ok("error");
    }
    let bytes = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
    events::item_fetched(url, bytes, 0);
    debug!(
        event = "slack_media_downloaded",
        file_id = file_id,
        bytes = bytes
    );
    Ok("downloaded")
}

/// Walk message records for `files[]` and download each. `records` is
/// the raw `messages` array from a `conversations.history` /
/// `conversations.replies` response.
pub async fn download_files_for_messages(
    messages: &[Value],
    media_dir: &Path,
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
    for f in &targets {
        let outcome = download_one_file(f, media_dir).await?;
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
