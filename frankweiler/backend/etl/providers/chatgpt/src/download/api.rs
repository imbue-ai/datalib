//! ChatGPT API transport. Every request goes through
//! [`frankweiler_etl::http::latchkey_curl`], which captures the full
//! response (status, every header, body) and supports playback from
//! disk fixtures. Mirrors `src/download/chatgpt_web.py:_curl_get`.
//!
//! Cloudflare TLS fingerprinting is `curl_impersonate`'s job; export
//! `LATCHKEY_CURL=/path/to/curl_impersonate-chrome` before running the
//! download binary live.
//!
//! Blob download itself happens in `download::mod` against the doltlite
//! `blobs` table; this module is transport-only.

use std::time::Duration;

use serde_json::Value;
use tracing::instrument;

use frankweiler_etl::events;
use frankweiler_etl::http::{latchkey_curl, HttpError, HttpRequest};

pub const BASE: &str = "https://chatgpt.com";
pub const LATCHKEY_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(thiserror::Error, Debug)]
pub enum ChatGPTError {
    /// The shared retry loop respected rate limits / backed off but the
    /// orchestrator's give-up policy tripped (`reason` says which bound).
    /// Surfaced distinctly from `Permanent` so the caller can stop fetching
    /// cleanly and the user resumes later via the incremental-skip path.
    #[error("rate-limited on {path}; gave up retrying: {reason}")]
    RateLimited { path: String, reason: String },
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
        let req = HttpRequest::get("chatgpt", &url)
            .header("Accept", "application/json")
            .timeout(LATCHKEY_TIMEOUT);
        // 429 retry — `Retry-After`, exponential backoff, and the give-up
        // bound — is handled centrally in the shared chokepoint. When it
        // gives up, surface `RateLimited` (not `Permanent`) so the caller
        // stops cleanly and the user resumes later.
        let resp = latchkey_curl(&req).await.map_err(|e: HttpError| match e {
            HttpError::GaveUp { reason, .. } => ChatGPTError::RateLimited {
                path: path.to_string(),
                reason,
            },
            other => ChatGPTError::Permanent(format!("GET {path}: {other}")),
        })?;
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
        let body_preview: String = resp.body_str().chars().take(300).collect();
        Err(ChatGPTError::Permanent(format!(
            "GET {path} -> HTTP {} cf-mitigated={:?} body={:?}",
            resp.status,
            resp.header("cf-mitigated"),
            body_preview
        )))
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
