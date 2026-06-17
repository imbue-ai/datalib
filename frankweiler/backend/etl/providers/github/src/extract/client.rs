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

use frankweiler_etl::http::{
    default_retryability, latchkey_curl_classified, HttpError, HttpRequest, HttpResponse,
    Retryability,
};

pub const BASE: &str = "https://api.github.com";
pub const LATCHKEY_TIMEOUT: Duration = Duration::from_secs(60);
pub const PER_PAGE: u32 = 100;

static LINK_NEXT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r#"<([^>]+)>;\s*rel="next""#).unwrap());

/// GitHub-specific retry classifier. The default classifier already treats
/// the *secondary* rate limit (HTTP 429) and 5xx as retryable; GitHub's
/// *primary* rate limit is instead a `403` with `x-ratelimit-remaining: 0`
/// plus an `x-ratelimit-reset` epoch telling us when the window resets. Map
/// that to a retry with the computed wait so the shared loop respects it.
fn github_retryability(resp: &HttpResponse) -> Retryability {
    if resp.status == 403 && resp.header("x-ratelimit-remaining") == Some("0") {
        let retry_after = resp.header("x-ratelimit-reset").and_then(|reset| {
            reset.parse::<i64>().ok().map(|ts| {
                let now = chrono::Utc::now().timestamp();
                Duration::from_secs(((ts - now).max(0) as u64).saturating_add(1))
            })
        });
        return Retryability::Retry { retry_after };
    }
    default_retryability(resp)
}

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
        // Rate-limit (429 + primary-limit 403) and 5xx retry — including the
        // `Retry-After` / `x-ratelimit-reset` waits — is handled centrally in
        // the shared chokepoint via `github_retryability`. A `GaveUp` here is
        // terminal and maps to `Permanent`.
        let resp = latchkey_curl_classified(&req, github_retryability)
            .await
            .map_err(|e: HttpError| GitHubError::Permanent(e.to_string()))?;
        self.network_ms
            .fetch_add(resp.duration_ms, Ordering::Relaxed);
        self.requests.fetch_add(1, Ordering::Relaxed);
        Ok(resp)
    }

    /// GET the definitive response (the chokepoint has already waited out any
    /// rate-limit / transient) and parse it. Returns the JSON body and
    /// response headers so callers can walk the `Link: rel=next` pagination
    /// chain. The HashMap type is preserved (rather than passing the http
    /// module's BTreeMap up) so callers in `mod.rs` need no change.
    pub async fn get(&self, url: &str) -> Result<(Value, HashMap<String, String>), GitHubError> {
        let resp = self.request_once(url).await?;
        let body = resp.body_str().into_owned();
        if (200..300).contains(&resp.status) {
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
        let preview: String = body.chars().take(300).collect();
        Err(GitHubError::Permanent(format!(
            "{url}: HTTP {} body={preview:?}",
            resp.status
        )))
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
