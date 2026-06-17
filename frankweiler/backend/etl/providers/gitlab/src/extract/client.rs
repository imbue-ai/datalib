//! GitLab REST API client (`gitlab.com/api/v4`). Every request goes
//! through [`frankweiler_etl::http::latchkey_curl`]. Latchkey injects
//! `PRIVATE-TOKEN: <token>` for the `gitlab` service.
//!
//! Port of `_call_gitlab_once` + `call_gitlab` + `paginate` in
//! `src/download/gitlab_web.py`.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

use frankweiler_etl::http::{latchkey_curl, HttpError, HttpRequest, HttpResponse};

pub const BASE: &str = "https://gitlab.com/api/v4";
pub const LATCHKEY_TIMEOUT: Duration = Duration::from_secs(60);
pub const PER_PAGE: u32 = 100;

static LINK_NEXT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r#"<([^>]+)>;\s*rel="next""#).unwrap());

#[derive(thiserror::Error, Debug)]
pub enum GitLabError {
    #[error("{0}")]
    Permanent(String),
}

pub struct GitLabClient {
    requests: AtomicU64,
    network_ms: AtomicU64,
}

impl Default for GitLabClient {
    fn default() -> Self {
        Self {
            requests: AtomicU64::new(0),
            network_ms: AtomicU64::new(0),
        }
    }
}

impl GitLabClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn request_count(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    async fn request_once(&self, url: &str) -> Result<HttpResponse, GitLabError> {
        let req = HttpRequest::get("gitlab", url).timeout(LATCHKEY_TIMEOUT);
        let resp = latchkey_curl(&req)
            .await
            .map_err(|e: HttpError| GitLabError::Permanent(e.to_string()))?;
        self.network_ms
            .fetch_add(resp.duration_ms, Ordering::Relaxed);
        self.requests.fetch_add(1, Ordering::Relaxed);
        Ok(resp)
    }

    /// Rate-limit (429) and transient (5xx) retry — including `Retry-After`
    /// / `ratelimit-reset` waits — is handled centrally in
    /// [`frankweiler_etl::http::latchkey_curl`], so this just parses the
    /// definitive response the chokepoint hands back.
    pub async fn get(&self, url: &str) -> Result<(Value, HashMap<String, String>), GitLabError> {
        let resp = self.request_once(url).await?;
        let body = resp.body_str().into_owned();
        if (200..300).contains(&resp.status) {
            let value: Value = if body.trim().is_empty() {
                Value::Null
            } else {
                serde_json::from_str(&body).map_err(|e| {
                    let preview: String = body.chars().take(200).collect();
                    GitLabError::Permanent(format!(
                        "{url}: HTTP {} but non-JSON: {e}; body[:200]={preview:?}",
                        resp.status
                    ))
                })?
            };
            let headers: HashMap<String, String> = resp.headers.into_iter().collect();
            return Ok((value, headers));
        }
        let preview: String = body.chars().take(300).collect();
        Err(GitLabError::Permanent(format!(
            "{url}: HTTP {} body={preview:?}",
            resp.status
        )))
    }

    /// Walk `Link: rel=next` pagination. GitLab returns top-level arrays
    /// for list endpoints and a single object for `/user` and similar.
    pub async fn paginate(&self, start_url: &str) -> Result<Vec<Value>, GitLabError> {
        let mut url = start_url.to_string();
        let mut out: Vec<Value> = Vec::new();
        loop {
            let (data, headers) = self.get(&url).await?;
            match &data {
                Value::Array(arr) => out.extend(arr.iter().cloned()),
                _ => {
                    out.push(data.clone());
                    return Ok(out);
                }
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
