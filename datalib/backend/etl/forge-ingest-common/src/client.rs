//! A forge's REST client. Every request goes through
//! [`datalib_etl::http::latchkey_curl_classified`], which handles the
//! latchkey subprocess, the rate-limit and transient retries, and
//! playback from disk fixtures. Latchkey injects the credential for the
//! service — don't add it here.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

use datalib_etl::http::{
    latchkey_curl_classified, HttpError, HttpRequest, HttpResponse, HttpService, LatchkeySettings,
    Retryability,
};

pub const LATCHKEY_TIMEOUT: Duration = Duration::from_secs(60);
pub const PER_PAGE: u32 = 100;

static LINK_NEXT_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r#"<([^>]+)>;\s*rel="next""#).unwrap());

#[derive(thiserror::Error, Debug)]
pub enum ForgeError {
    #[error("{0}")]
    Permanent(String),
}

pub struct ForgeClient {
    service: HttpService,
    /// Which responses the shared loop retries, and after how long.
    classify: fn(&HttpResponse) -> Retryability,
    requests: AtomicU64,
    network_ms: AtomicU64,
    /// The source's latchkey settings, forwarded onto every request this
    /// client issues (see `HttpRequest::latchkey`).
    latchkey: LatchkeySettings,
}

impl ForgeClient {
    pub fn new(
        service: HttpService,
        classify: fn(&HttpResponse) -> Retryability,
        latchkey: LatchkeySettings,
    ) -> Self {
        Self {
            service,
            classify,
            requests: AtomicU64::new(0),
            network_ms: AtomicU64::new(0),
            latchkey,
        }
    }

    pub fn request_count(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    async fn request_once(&self, url: &str) -> Result<HttpResponse, ForgeError> {
        let req = HttpRequest::get(self.service, url)
            .latchkey(self.latchkey.clone())
            .timeout(LATCHKEY_TIMEOUT);
        // A `GaveUp` from the shared loop is terminal: it has already
        // waited out every rate limit and transient it could.
        let resp = latchkey_curl_classified(&req, self.classify)
            .await
            .map_err(|e: HttpError| ForgeError::Permanent(e.to_string()))?;
        self.network_ms
            .fetch_add(resp.duration_ms, Ordering::Relaxed);
        self.requests.fetch_add(1, Ordering::Relaxed);
        Ok(resp)
    }

    /// GET and parse the definitive response. Returns the JSON body and
    /// the response headers, so callers can walk the `Link: rel=next`
    /// pagination chain.
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
    /// items. A page is a top-level array, or GitHub search's
    /// `{"items": [...]}`; any other object is one item, handed back
    /// alone.
    pub async fn paginate(&self, start_url: &str) -> Result<Vec<Value>, ForgeError> {
        let mut url = start_url.to_string();
        let mut out: Vec<Value> = Vec::new();
        loop {
            let (data, headers) = self.get(&url).await?;
            match &data {
                Value::Array(arr) => out.extend(arr.iter().cloned()),
                Value::Object(obj) => match obj.get("items").and_then(|v| v.as_array()) {
                    Some(items) => out.extend(items.iter().cloned()),
                    None => {
                        out.push(data.clone());
                        return Ok(out);
                    }
                },
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
