//! Notion official API client (`api.notion.com/v1`) via `latchkey curl`.
//!
//! Latchkey injects the Bearer token **and** the `Notion-Version` header
//! for the `notion` service. Do not set either here: a second
//! `Notion-Version` does not override the stored one, it concatenates
//! with it, and Notion rejects the pair with
//! `"instead was \"2022-06-28, 2026-03-11\""`. The version is a
//! property of the stored credential, so bumping it means
//! `latchkey auth set notion -H ... -H "Notion-Version: <new>"`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;

use datalib_etl::events;
use datalib_etl::http::{latchkey_curl, HttpError, HttpRequest, HttpService, LatchkeySettings};

pub const BASE: &str = "https://api.notion.com/v1";
pub const LATCHKEY_TIMEOUT: Duration = Duration::from_secs(180);
pub const PAGE_SIZE: u32 = 100;

#[derive(thiserror::Error, Debug)]
pub enum NotionOfficialError {
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("{0}")]
    Permanent(String),
}

pub struct NotionOfficialClient {
    requests: AtomicU64,
    network_ms: AtomicU64,
    /// The source's latchkey settings, forwarded onto every request this
    /// client issues (see `HttpRequest::latchkey`).
    latchkey: LatchkeySettings,
}

impl Default for NotionOfficialClient {
    fn default() -> Self {
        Self {
            requests: AtomicU64::new(0),
            network_ms: AtomicU64::new(0),
            latchkey: LatchkeySettings::default(),
        }
    }
}

impl NotionOfficialClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_latchkey(latchkey: LatchkeySettings) -> Self {
        Self {
            latchkey,
            ..Self::default()
        }
    }

    pub fn request_count(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    pub fn network_seconds(&self) -> f64 {
        (self.network_ms.load(Ordering::Relaxed) as f64) / 1000.0
    }

    #[tracing::instrument(skip(self, body), fields(method = %method, path = %path, total_ms))]
    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, NotionOfficialError> {
        let req_start = std::time::Instant::now();
        let url = format!("{BASE}{path}");
        // 429 / 5xx retry (with `Retry-After` / backoff) is handled centrally
        // in `latchkey_curl`; this issues the request once and parses the
        // definitive response.
        let req = match method {
            "GET" => HttpRequest::get(HttpService::Notion, &url)
                .header("Accept", "application/json")
                .latchkey(self.latchkey.clone())
                .timeout(LATCHKEY_TIMEOUT),
            "POST" => {
                let payload = body.map(|b| b.to_string().into_bytes()).unwrap_or_default();
                HttpRequest::post_json(HttpService::Notion, &url, payload)
                    .header("Accept", "application/json")
                    .latchkey(self.latchkey.clone())
                    .timeout(LATCHKEY_TIMEOUT)
            }
            other => {
                return Err(NotionOfficialError::Permanent(format!(
                    "{other} {path}: unsupported method"
                )));
            }
        };
        let resp = latchkey_curl(&req).await.map_err(|e: HttpError| {
            NotionOfficialError::Permanent(format!("{method} {path}: {e}"))
        })?;
        self.network_ms
            .fetch_add(resp.duration_ms, Ordering::Relaxed);
        self.requests.fetch_add(1, Ordering::Relaxed);

        let body_text = resp.body_str().into_owned();
        let status = resp.status;
        if status == 200 {
            let value: Value = serde_json::from_str(&body_text).map_err(|e| {
                let preview: String = body_text.chars().take(200).collect();
                NotionOfficialError::Permanent(format!(
                    "{method} {path}: HTTP 200 but non-JSON: {e}; body[:200]={preview:?}"
                ))
            })?;
            events::item_fetched(&url, resp.body.len() as u64, resp.duration_ms);
            tracing::Span::current().record("total_ms", req_start.elapsed().as_millis() as u64);
            return Ok(value);
        }
        if status == 403 {
            return Err(NotionOfficialError::Forbidden(format!(
                "{method} {path} -> HTTP 403"
            )));
        }
        let preview: String = body_text.chars().take(300).collect();
        Err(NotionOfficialError::Permanent(format!(
            "{method} {path}: HTTP {status} body={preview:?}"
        )))
    }

    pub async fn get_page(&self, page_id: &str) -> Result<Value, NotionOfficialError> {
        self.request("GET", &format!("/pages/{page_id}"), None)
            .await
    }

    pub async fn get_block_children(
        &self,
        block_id: &str,
        start_cursor: Option<&str>,
    ) -> Result<Value, NotionOfficialError> {
        let mut q = format!("?page_size={PAGE_SIZE}");
        if let Some(c) = start_cursor {
            q.push_str("&start_cursor=");
            q.push_str(c);
        }
        self.request("GET", &format!("/blocks/{block_id}/children{q}"), None)
            .await
    }

    /// The page body, already rendered by Notion as enhanced markdown.
    ///
    /// This replaces walking the block tree: one request instead of one
    /// per container block. The response carries `truncated` and
    /// `unresolved_block_ids` for anything it could not inline — see
    /// [`crate::download::markdown`] for what those mean and which of
    /// them are worth a follow-up.
    pub async fn get_page_markdown(&self, page_id: &str) -> Result<Value, NotionOfficialError> {
        self.request("GET", &format!("/pages/{page_id}/markdown"), None)
            .await
    }

    /// One user. There is deliberately no list-all counterpart:
    /// `GET /v1/users` is unavailable to personal access tokens, which
    /// is what this provider authenticates with, so users are resolved
    /// one id at a time and cached.
    pub async fn get_user(&self, user_id: &str) -> Result<Value, NotionOfficialError> {
        self.request("GET", &format!("/users/{user_id}"), None)
            .await
    }

    /// One block. Used only to recover the text a comment is anchored
    /// to — the block tree itself is not mirrored.
    pub async fn get_block(&self, block_id: &str) -> Result<Value, NotionOfficialError> {
        self.request("GET", &format!("/blocks/{block_id}"), None)
            .await
    }

    /// One page of `POST /v1/search`, newest-edited first.
    ///
    /// With a personal access token this enumerates everything its
    /// creator can see, which is what removes the need for
    /// hand-configured seeds. Results are page and data_source objects,
    /// and page objects come back complete — properties included — so
    /// for a database row with no body this single response is the
    /// whole record.
    pub async fn search(
        &self,
        start_cursor: Option<&str>,
        in_trash: bool,
    ) -> Result<Value, NotionOfficialError> {
        let mut body = serde_json::json!({
            "page_size": PAGE_SIZE,
            "sort": { "timestamp": "last_edited_time", "direction": "descending" },
        });
        if in_trash {
            body["filter"] = serde_json::json!({ "in_trash": true });
        }
        if let Some(c) = start_cursor {
            body["start_cursor"] = Value::String(c.to_string());
        }
        self.request("POST", "/search", Some(&body)).await
    }

    pub async fn get_comments(
        &self,
        block_id: &str,
        start_cursor: Option<&str>,
    ) -> Result<Value, NotionOfficialError> {
        let mut q = format!("?block_id={block_id}&page_size={PAGE_SIZE}");
        if let Some(c) = start_cursor {
            q.push_str("&start_cursor=");
            q.push_str(c);
        }
        self.request("GET", &format!("/comments{q}"), None).await
    }
}
