//! GitHub REST API client (`api.github.com`). Every request goes through
//! [`frankweiler_etl::http::latchkey_curl`], which handles the latchkey
//! subprocess and supports playback from disk fixtures. Latchkey injects
//! the `Authorization: Bearer <token>` header for the `github` service —
//! don't add it here.
//!
//! Port of `_call_github_once` + `call_github` + `paginate` in
//! `src/download/github_web.py`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

use frankweiler_etl::http::{latchkey_curl, HttpError, HttpRequest, HttpResponse};

pub const BASE: &str = "https://api.github.com";
pub const LATCHKEY_TIMEOUT: Duration = Duration::from_secs(60);
pub const PER_PAGE: u32 = 100;
const RETRY_MAX: u32 = 7;
const RETRY_INITIAL_BACKOFF_MS: u64 = 2_000;
const RETRY_MAX_BACKOFF_MS: u64 = 120_000;

static LINK_NEXT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r#"<([^>]+)>;\s*rel="next""#).unwrap());

#[derive(thiserror::Error, Debug)]
pub enum GitHubError {
    #[error("{0}")]
    Permanent(String),
}

pub struct GitHubClient {
    requests: AtomicU64,
    network_ms: AtomicU64,
}

impl Default for GitHubClient {
    fn default() -> Self {
        Self {
            requests: AtomicU64::new(0),
            network_ms: AtomicU64::new(0),
        }
    }
}

impl GitHubClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn request_count(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    async fn request_once(&self, url: &str) -> Result<HttpResponse, GitHubError> {
        let req = HttpRequest::get("github", url).timeout(LATCHKEY_TIMEOUT);
        let resp = latchkey_curl(&req)
            .await
            .map_err(|e: HttpError| GitHubError::Permanent(e.to_string()))?;
        self.network_ms
            .fetch_add(resp.duration_ms, Ordering::Relaxed);
        self.requests.fetch_add(1, Ordering::Relaxed);
        Ok(resp)
    }

    /// GET with exponential backoff on rate-limit / transient failures.
    /// Returns the parsed JSON body and response headers so callers can
    /// walk the `Link: rel=next` pagination chain. The HashMap type is
    /// preserved (rather than passing the http module's BTreeMap up) so
    /// callers in `mod.rs` need no change.
    pub async fn get(&self, url: &str) -> Result<(Value, HashMap<String, String>), GitHubError> {
        let mut backoff_ms = RETRY_INITIAL_BACKOFF_MS;
        for attempt in 0..=RETRY_MAX {
            let resp = self.request_once(url).await?;
            let body = resp.body_str().into_owned();
            if resp.status >= 200 && resp.status < 300 {
                let value: Value = if body.trim().is_empty() {
                    Value::Null
                } else {
                    serde_json::from_str(&body).map_err(|e| {
                        let preview: String = body.chars().take(200).collect();
                        GitHubError::Permanent(format!(
                            "{url}: HTTP {} but non-JSON: {e}; body[:200]={preview:?}",
                            resp.status
                        ))
                    })?
                };
                let headers: HashMap<String, String> = resp.headers.into_iter().collect();
                return Ok((value, headers));
            }
            // 403 + x-ratelimit-remaining: 0  → primary rate limit (retry)
            // 429                              → secondary rate limit (retry)
            // 5xx                              → transient
            let is_rate_limit = resp.status == 429
                || (resp.status == 403
                    && resp
                        .header("x-ratelimit-remaining")
                        .map(|v| v == "0")
                        .unwrap_or(false));
            let is_transient = matches!(resp.status, 502..=504);
            if is_rate_limit || is_transient {
                if attempt == RETRY_MAX {
                    return Err(GitHubError::Permanent(format!(
                        "{url}: HTTP {} after {attempt} retries",
                        resp.status
                    )));
                }
                let mut sleep_ms = backoff_ms;
                if let Some(retry_after) = resp.header("retry-after") {
                    if let Ok(s) = retry_after.parse::<u64>() {
                        sleep_ms = sleep_ms.max(s * 1000);
                    }
                } else if let Some(reset) = resp.header("x-ratelimit-reset") {
                    if let Ok(ts) = reset.parse::<i64>() {
                        let now = chrono::Utc::now().timestamp();
                        let delta_s = (ts - now).max(0) as u64;
                        sleep_ms = sleep_ms.max(delta_s.saturating_add(1) * 1000);
                    }
                }
                sleep_ms = sleep_ms.min(RETRY_MAX_BACKOFF_MS);
                tracing::warn!(
                    url,
                    status = resp.status,
                    attempt = attempt + 1,
                    sleep_ms,
                    "rate-limited/transient; sleeping"
                );
                tokio::time::sleep(Duration::from_millis(sleep_ms)).await;
                backoff_ms = (backoff_ms * 2).min(RETRY_MAX_BACKOFF_MS);
                continue;
            }
            let preview: String = body.chars().take(300).collect();
            return Err(GitHubError::Permanent(format!(
                "{url}: HTTP {} body={preview:?}",
                resp.status
            )));
        }
        unreachable!()
    }

    /// Walk `Link: rel=next` pagination until exhausted, accumulating items.
    /// Handles two response shapes:
    /// - top-level array (most list endpoints)
    /// - `{"items": [...]}` (search endpoints)
    pub async fn paginate(&self, start_url: &str) -> Result<Vec<Value>, GitHubError> {
        let mut url = start_url.to_string();
        let mut out: Vec<Value> = Vec::new();
        loop {
            let (data, headers) = self.get(&url).await?;
            match &data {
                Value::Array(arr) => out.extend(arr.iter().cloned()),
                Value::Object(obj) => {
                    if let Some(items) = obj.get("items").and_then(|v| v.as_array()) {
                        out.extend(items.iter().cloned());
                    } else {
                        // Single-object response, hand back as one item.
                        out.push(data.clone());
                        return Ok(out);
                    }
                }
                _ => return Ok(out),
            }
            let Some(link) = headers.get("link") else {
                return Ok(out);
            };
            let Some(m) = LINK_NEXT_RE.captures(link) else {
                return Ok(out);
            };
            url = m.get(1).unwrap().as_str().to_string();
        }
    }
}

/// If `LATCHKEY_CURL` is unset, point it at the in-tree
/// `latchkey-curl-shim`. GitHub's `api.github.com` accepts vanilla curl
/// fine, but every other provider in the workspace needs the shim — keep
/// the behaviour uniform.
pub fn auto_set_latchkey_curl() {
    if std::env::var_os("LATCHKEY_CURL").is_some() {
        return;
    }
    for c in [
        "target/debug/latchkey-curl-shim",
        "target/release/latchkey-curl-shim",
        "frankweiler/backend/target/debug/latchkey-curl-shim",
        "frankweiler/backend/target/release/latchkey-curl-shim",
    ] {
        if std::path::Path::new(c).exists() {
            std::env::set_var("LATCHKEY_CURL", c);
            return;
        }
    }
}
