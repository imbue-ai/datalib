//! GitHub REST API client (`api.github.com`) via `latchkey curl`.
//! Latchkey injects the `Authorization: Bearer <token>` header for the
//! `github` service. Don't add it here.
//!
//! Port of `_call_github_once` + `call_github` + `paginate` in
//! `src/download/github_web.py`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;
use tokio::process::Command;

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

#[derive(Debug)]
struct RawResponse {
    status: u16,
    headers: std::collections::HashMap<String, String>,
    body: String,
}

impl GitHubClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn request_count(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    async fn request_once(&self, url: &str) -> Result<RawResponse, GitHubError> {
        let t0 = std::time::Instant::now();
        let proc = tokio::time::timeout(
            LATCHKEY_TIMEOUT,
            Command::new("latchkey")
                .args(["curl", "-sS", "-D", "-", url])
                .output(),
        )
        .await
        .map_err(|_| GitHubError::Permanent(format!("{url}: latchkey curl timed out")))?
        .map_err(|e| GitHubError::Permanent(format!("{url}: spawn: {e}")))?;
        let elapsed_ms = t0.elapsed().as_millis() as u64;
        self.network_ms.fetch_add(elapsed_ms, Ordering::Relaxed);
        self.requests.fetch_add(1, Ordering::Relaxed);

        if !proc.status.success() {
            return Err(GitHubError::Permanent(format!(
                "{url}: latchkey exit {} stderr={:?}",
                proc.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&proc.stderr)
                    .chars()
                    .take(200)
                    .collect::<String>()
            )));
        }
        let stdout = String::from_utf8_lossy(&proc.stdout).into_owned();
        // -D - prepends headers. Split on the first blank line.
        let (head, body) = stdout
            .split_once("\r\n\r\n")
            .or_else(|| stdout.split_once("\n\n"))
            .map(|(h, b)| (h.to_string(), b.to_string()))
            .unwrap_or_else(|| (stdout.clone(), String::new()));

        let mut headers: std::collections::HashMap<String, String> = Default::default();
        let mut status: u16 = 0;
        for (i, line) in head.lines().enumerate() {
            if i == 0 {
                // "HTTP/2 200" or "HTTP/1.1 200 OK"
                let parts: Vec<&str> = line.split_whitespace().collect();
                if parts.len() >= 2 {
                    status = parts[1].parse().unwrap_or(0);
                }
                continue;
            }
            if let Some((k, v)) = line.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }
        Ok(RawResponse {
            status,
            headers,
            body,
        })
    }

    /// GET with exponential backoff on rate-limit / transient failures.
    /// Returns the parsed JSON body and the response headers (so callers
    /// can walk the `Link: rel=next` pagination chain).
    pub async fn get(
        &self,
        url: &str,
    ) -> Result<(Value, std::collections::HashMap<String, String>), GitHubError> {
        let mut backoff_ms = RETRY_INITIAL_BACKOFF_MS;
        for attempt in 0..=RETRY_MAX {
            let resp = self.request_once(url).await?;
            if resp.status >= 200 && resp.status < 300 {
                let value: Value = if resp.body.trim().is_empty() {
                    Value::Null
                } else {
                    serde_json::from_str(&resp.body).map_err(|e| {
                        let preview: String = resp.body.chars().take(200).collect();
                        GitHubError::Permanent(format!(
                            "{url}: HTTP {} but non-JSON: {e}; body[:200]={preview:?}",
                            resp.status
                        ))
                    })?
                };
                return Ok((value, resp.headers));
            }
            // 403 + x-ratelimit-remaining: 0  → primary rate limit (retry)
            // 429                              → secondary rate limit (retry)
            // 5xx                              → transient
            let is_rate_limit = resp.status == 429
                || (resp.status == 403
                    && resp
                        .headers
                        .get("x-ratelimit-remaining")
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
                if let Some(retry_after) = resp.headers.get("retry-after") {
                    if let Ok(s) = retry_after.parse::<u64>() {
                        sleep_ms = sleep_ms.max(s * 1000);
                    }
                } else if let Some(reset) = resp.headers.get("x-ratelimit-reset") {
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
            let preview: String = resp.body.chars().take(300).collect();
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
