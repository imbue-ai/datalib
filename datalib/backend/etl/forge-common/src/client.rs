//! REST client shared by the forge providers.
//!
//! Every request goes through [`datalib_etl::http::latchkey_curl_classified`],
//! which owns the latchkey subprocess, the rate-limit / transient retry
//! loop, and playback from disk fixtures. Latchkey injects the auth
//! header for the named service — `Authorization: Bearer <token>` for
//! `github`, `PRIVATE-TOKEN: <token>` for `gitlab` — so no caller adds
//! one.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

use datalib_etl::http::{
    latchkey_curl_classified, HttpError, HttpRequest, HttpResponse, Retryability,
};

pub const LATCHKEY_TIMEOUT: Duration = Duration::from_secs(60);
pub const PER_PAGE: u32 = 100;

static LINK_NEXT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r#"<([^>]+)>;\s*rel="next""#).unwrap());

/// A request that no amount of retrying will fix.
///
/// The shared chokepoint has already waited out anything transient by
/// the time a response reaches this crate, so every error surfacing
/// here is terminal — hence the single variant.
#[derive(thiserror::Error, Debug)]
pub enum ForgeError {
    #[error("{0}")]
    Permanent(String),
}

/// A counted REST client for one forge.
///
/// `service` is the latchkey service name (`"github"`, `"gitlab"`).
/// `classify` decides which responses are worth retrying; pass
/// [`datalib_etl::http::default_retryability`] unless the forge signals
/// rate limiting in a way the status code alone doesn't capture.
pub struct ForgeClient {
    service: &'static str,
    classify: fn(&HttpResponse) -> Retryability,
    requests: AtomicU64,
    network_ms: AtomicU64,
}

impl ForgeClient {
    pub fn new(service: &'static str, classify: fn(&HttpResponse) -> Retryability) -> Self {
        Self {
            service,
            classify,
            requests: AtomicU64::new(0),
            network_ms: AtomicU64::new(0),
        }
    }

    pub fn request_count(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    /// Total time spent waiting on the network, in milliseconds.
    pub fn network_ms(&self) -> u64 {
        self.network_ms.load(Ordering::Relaxed)
    }

    async fn request_once(&self, url: &str) -> Result<HttpResponse, ForgeError> {
        let req = HttpRequest::get(self.service, url).timeout(LATCHKEY_TIMEOUT);
        let resp = latchkey_curl_classified(&req, self.classify)
            .await
            .map_err(|e: HttpError| ForgeError::Permanent(e.to_string()))?;
        self.network_ms
            .fetch_add(resp.duration_ms, Ordering::Relaxed);
        self.requests.fetch_add(1, Ordering::Relaxed);
        Ok(resp)
    }

    /// GET the definitive response and parse it.
    ///
    /// Returns the JSON body plus the response headers, so callers can
    /// walk the `Link: rel=next` chain themselves when they need more
    /// control than [`Self::paginate`] gives.
    pub async fn get(&self, url: &str) -> Result<(Value, HashMap<String, String>), ForgeError> {
        let resp = self.request_once(url).await?;
        let body = resp.body_str().into_owned();
        if (200..300).contains(&resp.status) {
            let value: Value = if body.trim().is_empty() {
                Value::Null
            } else {
                serde_json::from_str(&body).map_err(|e| {
                    let preview: String = body.chars().take(200).collect();
                    ForgeError::Permanent(format!(
                        "{url}: HTTP {} but non-JSON: {e}; body[:200]={preview:?}",
                        resp.status
                    ))
                })?
            };
            let headers: HashMap<String, String> = resp.headers.into_iter().collect();
            return Ok((value, headers));
        }
        let preview: String = body.chars().take(300).collect();
        Err(ForgeError::Permanent(format!(
            "{url}: HTTP {} body={preview:?}",
            resp.status
        )))
    }

    /// Walk `Link: rel=next` pagination until exhausted, accumulating
    /// items. Handles the three response shapes the two forges return:
    ///
    ///   * top-level array — most list endpoints on both forges;
    ///   * `{"items": [...]}` — GitHub's search endpoints;
    ///   * any other single value — `/user` and friends, handed back
    ///     as a one-element result.
    pub async fn paginate(&self, start_url: &str) -> Result<Vec<Value>, ForgeError> {
        let mut url = start_url.to_string();
        let mut out: Vec<Value> = Vec::new();
        loop {
            let (data, headers) = self.get(&url).await?;
            match &data {
                Value::Array(arr) => out.extend(arr.iter().cloned()),
                Value::Object(obj) if obj.contains_key("items") => {
                    // Search endpoints wrap the page in `items`; a
                    // non-array `items` is malformed, so treat it as
                    // an empty page rather than panicking.
                    if let Some(items) = obj.get("items").and_then(|v| v.as_array()) {
                        out.extend(items.iter().cloned());
                    }
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_next_extracts_the_next_url() {
        let link = r#"<https://api.example.com/x?page=2>; rel="next", \
                      <https://api.example.com/x?page=9>; rel="last""#;
        let m = LINK_NEXT_RE.captures(link).expect("should match");
        assert_eq!(
            m.get(1).unwrap().as_str(),
            "https://api.example.com/x?page=2"
        );
    }

    #[test]
    fn link_next_absent_when_only_prev_and_last() {
        let link = r#"<https://api.example.com/x?page=1>; rel="prev", \
                      <https://api.example.com/x?page=9>; rel="last""#;
        assert!(LINK_NEXT_RE.captures(link).is_none());
    }

    /// Counters start at zero so a provider's summary line reports
    /// "0 requests" for a fully-skipped run rather than garbage.
    #[test]
    fn counters_start_at_zero() {
        let c = ForgeClient::new("github", datalib_etl::http::default_retryability);
        assert_eq!(c.request_count(), 0);
        assert_eq!(c.network_ms(), 0);
    }
}
