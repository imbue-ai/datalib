//! ChatGPT API transport. Every request goes through
//! [`datalib_etl_web::http::latchkey_curl`], which captures the full
//! response (status, every header, body) and supports playback from
//! disk fixtures. Mirrors `src/ingest/chatgpt_web.py:_curl_get`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tracing::instrument;

use datalib_etl::events;
use datalib_etl_web::http::{latchkey_curl, HttpError, HttpRequest, HttpService, LatchkeySettings};

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

/// Requests go out from `&self`, so the fetch loop's `Fetcher` can hold
/// one client; the counts are atomics for the same reason.
#[derive(Default)]
pub struct ChatGPTClient {
    requests: AtomicU64,
    network_ms: AtomicU64,
    /// The source's latchkey settings, forwarded onto every request this
    /// client issues (see `HttpRequest::latchkey`).
    latchkey: LatchkeySettings,
}

impl ChatGPTClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_latchkey(latchkey: LatchkeySettings) -> Self {
        Self {
            latchkey,
            ..Self::default()
        }
    }

    /// The identity this client authenticates as, for the attachment
    /// bytes behind a signed URL, which the fetch builds as its own
    /// `HttpRequest`.
    pub fn latchkey(&self) -> &LatchkeySettings {
        &self.latchkey
    }

    pub fn requests(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    pub fn network_seconds(&self) -> f64 {
        self.network_ms.load(Ordering::Relaxed) as f64 / 1000.0
    }

    /// Counts a request this client did not make through [`Self::get`].
    pub fn count(&self, duration_ms: u64) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.network_ms.fetch_add(duration_ms, Ordering::Relaxed);
    }

    #[instrument(skip(self), fields(path = path))]
    pub async fn get(&self, path: &str) -> Result<Value, ChatGPTError> {
        let url = format!("{BASE}{path}");
        let req = HttpRequest::get(HttpService::Chatgpt, &url)
            .header("Accept", "application/json")
            .latchkey(self.latchkey.clone())
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
        self.count(resp.duration_ms);

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

    pub async fn me(&self) -> Result<Value, ChatGPTError> {
        self.get("/backend-api/me").await
    }

    pub async fn list_conversations_page(
        &self,
        offset: usize,
        limit: usize,
    ) -> Result<Value, ChatGPTError> {
        self.get(&format!(
            "/backend-api/conversations?offset={offset}&limit={limit}&order=updated"
        ))
        .await
    }

    pub async fn get_conversation(&self, conv_id: &str) -> Result<Value, ChatGPTError> {
        self.get(&format!("/backend-api/conversation/{conv_id}"))
            .await
    }
}
