//! ChatGPT API transport. Every request goes through
//! [`frankweiler_etl::http::latchkey_curl`], which captures the full
//! response (status, every header, body) and supports playback from
//! disk fixtures. Mirrors `src/download/chatgpt_web.py:_curl_get`.
//!
//! Cloudflare TLS fingerprinting is `curl_impersonate`'s job; export
//! `LATCHKEY_CURL=/path/to/curl_impersonate-chrome` before running the
//! download binary live.
//!
//! Blob download itself happens in `extract::mod` against the doltlite
//! `blobs` table; this module is transport-only.

use std::time::Duration;

use serde_json::Value;
use tokio::time::sleep;
use tracing::{instrument, warn};

use frankweiler_etl::http::{latchkey_curl, HttpError, HttpRequest};
use frankweiler_etl::obs::events;

pub const BASE: &str = "https://chatgpt.com";
pub const LATCHKEY_TIMEOUT: Duration = Duration::from_secs(120);
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
