//! ChatGPT API transport. Every request goes through
//! [`frankweiler_etl::http::latchkey_curl`], which captures the full
//! response (status, every header, body) and supports playback from
//! disk fixtures. Mirrors `src/download/chatgpt_web.py:_curl_get`.
//!
//! Cloudflare TLS fingerprinting is `curl_impersonate`'s job; export
//! `LATCHKEY_CURL=/path/to/curl_impersonate-chrome` before running the
//! download binary live.

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

pub const BASE: &str = "https://chatgpt.com";
pub const LATCHKEY_TIMEOUT: Duration = Duration::from_secs(120);
pub const LATCHKEY_FILE_TIMEOUT: Duration = Duration::from_secs(600);
/// When ChatGPT 429s us it tends to do so for many minutes. Give up
/// after this much total backoff so the user can resume later via the
/// incremental-skip path.
pub const RATE_LIMIT_GIVE_UP_AFTER: Duration = Duration::from_secs(300);

#[derive(thiserror::Error, Debug)]
pub enum ChatGPTError {
    #[error("rate-limited on {path}; gave up after {waited_secs:.0}s of backoff")]
    RateLimited { path: String, waited_secs: f64 },
    #[error("{0}")]
    Permanent(String),
}

pub struct ChatGPTClient {
    pub requests: u64,
    pub network_seconds: f64,
}

impl Default for ChatGPTClient {
    fn default() -> Self {
        Self {
            requests: 0,
            network_seconds: 0.0,
        }
    }
}

impl ChatGPTClient {
    pub fn new() -> Self {
        Self::default()
    }

    #[instrument(skip(self), fields(path = path))]
    pub async fn get(&mut self, path: &str) -> Result<Value, ChatGPTError> {
        let url = format!("{BASE}{path}");
        let mut waited = Duration::from_secs(0);
        loop {
            let req = HttpRequest::get("chatgpt", &url)
                .header("Accept", "application/json")
                .timeout(LATCHKEY_TIMEOUT);
            let resp = latchkey_curl(&req)
                .await
                .map_err(|e: HttpError| ChatGPTError::Permanent(format!("GET {path}: {e}")))?;
            self.network_seconds += (resp.duration_ms as f64) / 1000.0;
            self.requests += 1;

            if resp.status == 200 {
                let body = resp.body_str();
                let value: Value = serde_json::from_str(&body).map_err(|e| {
                    let preview: String = body.chars().take(200).collect();
                    ChatGPTError::Permanent(format!(
                        "GET {path}: 200 but non-JSON body: {e}; body[:200]={preview:?}"
                    ))
                })?;
                events::item_fetched(&url, resp.body.len() as u64, resp.duration_ms);
                return Ok(value);
            }
            if resp.status == 429 {
                let wait = backoff_from_retry_after(resp.header("retry-after"), waited);
                if waited + wait > RATE_LIMIT_GIVE_UP_AFTER {
                    return Err(ChatGPTError::RateLimited {
                        path: path.to_string(),
                        waited_secs: waited.as_secs_f64(),
                    });
                }
                warn!(
                    event = "chatgpt_rate_limited",
                    path = path,
                    wait_secs = wait.as_secs_f64(),
                    waited_so_far_secs = waited.as_secs_f64(),
                );
                sleep(wait).await;
                waited += wait;
                continue;
            }
            let body_preview: String = resp.body_str().chars().take(300).collect();
            return Err(ChatGPTError::Permanent(format!(
                "GET {path} -> HTTP {} cf-mitigated={:?} body={:?}",
                resp.status,
                resp.header("cf-mitigated"),
                body_preview
            )));
        }
    }

    pub async fn me(&mut self) -> Result<Value, ChatGPTError> {
        self.get("/backend-api/me").await
    }

    pub async fn list_conversations_page(
        &mut self,
        offset: usize,
        limit: usize,
    ) -> Result<Value, ChatGPTError> {
        self.get(&format!(
            "/backend-api/conversations?offset={offset}&limit={limit}&order=updated"
        ))
        .await
    }

    pub async fn get_conversation(&mut self, conv_id: &str) -> Result<Value, ChatGPTError> {
        self.get(&format!("/backend-api/conversation/{conv_id}"))
            .await
    }
}

fn backoff_from_retry_after(retry_after: Option<&str>, waited: Duration) -> Duration {
    if let Some(ra) = retry_after {
        if let Ok(secs) = ra.parse::<f64>() {
            if secs.is_finite() && secs >= 0.0 {
                return Duration::from_secs_f64(secs.min(300.0));
            }
        }
        return Duration::from_secs(30);
    }
    // 5 * 2^n exponential, capped at 60s, where n grows every 5s of prior
    // backoff. Matches the Python heuristic.
    let n = (waited.as_secs() / 5).min(4) as u32;
    let secs = 5u64.saturating_mul(1u64 << n).min(60);
    Duration::from_secs(secs)
}

// ---------------------------------------------------------------------------
// File-download path. ChatGPT serves attachment bytes via
// `/backend-api/files/<file_id>/download`, which returns JSON with a
// short-lived `download_url` (Azure blob storage). We fetch the
// metadata JSON via latchkey (auth header attached automatically),
// then `curl -fSL` the signed URL directly — Azure rejects the
// chatgpt cookie, so we deliberately drop auth on the second hop.
// ---------------------------------------------------------------------------

/// Download a single ChatGPT attachment into
/// `<media_dir>/<file_id>/<safe(name)>`. Returns one of
/// `downloaded` / `skipped` / `error`. Never panics — errors are logged
/// and counted by the caller so a single bad file doesn't tank a sync.
pub async fn download_one_file(
    client: &mut ChatGPTClient,
    file_id: &str,
    name: Option<&str>,
    media_dir: &Path,
) -> Result<&'static str> {
    let safe = safe_filename(name, file_id);
    let target_dir = media_dir.join(file_id);
    let target = target_dir.join(&safe);
    if let Ok(meta) = std::fs::metadata(&target) {
        if meta.len() > 0 {
            return Ok("skipped");
        }
    }
    std::fs::create_dir_all(&target_dir)
        .with_context(|| format!("mkdir {}", target_dir.display()))?;

    // Step 1: metadata fetch via latchkey (auth attached).
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
            return Ok("error");
        }
    };
    let signed = match meta.get("download_url").and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            warn!(
                event = "chatgpt_media_no_download_url",
                file_id = file_id,
                meta_keys = ?meta
                    .as_object()
                    .map(|o| o.keys().cloned().collect::<Vec<_>>())
                    .unwrap_or_default(),
            );
            return Ok("error");
        }
    };

    // Step 2: signed-url GET. The signed URL points at Cloudflare-fronted
    // storage that rejects vanilla curl's TLS fingerprint (exit 56). Go
    // through `latchkey curl`, which runs the in-tree Chrome-impersonating
    // shim. latchkey only attaches auth for URLs in a registered service's
    // baseApiUrls — the signed host isn't one, so we get an unauth'd hop
    // (which is what we want; the URL signature carries authorization).
    let mut cmd = latchkey_tokio_command();
    cmd.arg("curl")
        .arg("-fSL")
        .arg("-o")
        .arg(&target)
        .arg(&signed);
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
            event = "chatgpt_media_failed",
            file_id = file_id,
            name = %safe,
            exit = proc.status.code().unwrap_or(-1),
            stderr = %tail.trim(),
        );
        return Ok("error");
    }
    let bytes = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
    events::item_fetched(&signed, bytes, 0);
    debug!(
        event = "chatgpt_media_downloaded",
        file_id = file_id,
        bytes = bytes
    );
    Ok("downloaded")
}

/// Walk a conversation tree's `mapping.*.message.metadata.attachments[]`
/// and pull each unique file. Returns outcome counts.
pub async fn download_attachments_for_conversation(
    client: &mut ChatGPTClient,
    conv: &Value,
    media_dir: &Path,
) -> Result<BTreeMap<String, usize>> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for k in ["downloaded", "skipped", "error"] {
        counts.insert(k.to_string(), 0);
    }
    let mapping = match conv.get("mapping").and_then(|v| v.as_object()) {
        Some(m) => m,
        None => return Ok(counts),
    };
    // Dedupe by file_id within a conversation: identical assets often
    // appear under multiple parts (asset_pointer + attachments mirror).
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut targets: Vec<(String, Option<String>)> = Vec::new();
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
                    targets.push((id.to_string(), name));
                }
            }
        }
    }
    for (id, name) in targets {
        let outcome = download_one_file(client, &id, name.as_deref(), media_dir).await?;
        *counts.entry(outcome.to_string()).or_insert(0) += 1;
    }
    debug!(
        event = "chatgpt_media_summary",
        downloaded = counts.get("downloaded").copied().unwrap_or(0),
        skipped = counts.get("skipped").copied().unwrap_or(0),
        errors = counts.get("error").copied().unwrap_or(0),
    );
    Ok(counts)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backoff_grows_with_waited() {
        let b0 = backoff_from_retry_after(None, Duration::from_secs(0));
        let b1 = backoff_from_retry_after(None, Duration::from_secs(15));
        assert!(b1 >= b0);
        assert!(b1 <= Duration::from_secs(60));
    }

    #[test]
    fn retry_after_seconds_parsed() {
        let d = backoff_from_retry_after(Some("7"), Duration::from_secs(0));
        assert_eq!(d, Duration::from_secs(7));
    }
}
