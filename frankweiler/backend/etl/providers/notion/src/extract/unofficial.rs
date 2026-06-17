//! Notion unofficial web API client (`www.notion.so/api/v3`), cookie-auth
//! via the `notion_unofficial` latchkey service. Used only for the
//! handful of endpoints the public API doesn't expose:
//! `loadUserContent`, `getSpaces`, `getNotificationLog`.
//!
//! Responses can be large (notification logs in particular), so the
//! body is captured to a tempfile via `-o` rather than via stdout.
//! Port of `NotionUnofficialClient` in `src/download/notion_official.py`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;

use frankweiler_etl::events;
use frankweiler_etl::http::{latchkey_curl, HttpError, HttpRequest};

pub const BASE: &str = "https://www.notion.so/api/v3";
pub const LATCHKEY_TIMEOUT: Duration = Duration::from_secs(180);

#[derive(thiserror::Error, Debug)]
pub enum NotionUnofficialError {
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("{0}")]
    Permanent(String),
}

pub struct NotionUnofficialClient {
    requests: AtomicU64,
    network_ms: AtomicU64,
}

impl Default for NotionUnofficialClient {
    fn default() -> Self {
        Self {
            requests: AtomicU64::new(0),
            network_ms: AtomicU64::new(0),
        }
    }
}

impl NotionUnofficialClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn request_count(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    pub fn network_seconds(&self) -> f64 {
        (self.network_ms.load(Ordering::Relaxed) as f64) / 1000.0
    }

    async fn post(&self, method: &str, body: &Value) -> Result<Value, NotionUnofficialError> {
        let url = format!("{BASE}/{method}");
        let payload = body.to_string().into_bytes();
        // 429 / 5xx retry (with `Retry-After` / backoff) is handled centrally
        // in `latchkey_curl`; this issues the request once and parses the
        // definitive response.
        let req = HttpRequest::post_json("notion_unofficial", &url, payload)
            .header("Accept", "application/json")
            .timeout(LATCHKEY_TIMEOUT);
        let resp = latchkey_curl(&req)
            .await
            .map_err(|e: HttpError| NotionUnofficialError::Permanent(format!("{method}: {e}")))?;
        self.network_ms
            .fetch_add(resp.duration_ms, Ordering::Relaxed);
        self.requests.fetch_add(1, Ordering::Relaxed);

        let resp_text = resp.body_str().into_owned();
        let status = resp.status;
        if status == 200 {
            let value: Value = serde_json::from_str(&resp_text).map_err(|e| {
                let preview: String = resp_text.chars().take(200).collect();
                NotionUnofficialError::Permanent(format!(
                    "{method}: HTTP 200 but non-JSON: {e}; body[:200]={preview:?}"
                ))
            })?;
            events::item_fetched(&url, resp.body.len() as u64, resp.duration_ms);
            return Ok(value);
        }
        if status == 403 {
            return Err(NotionUnofficialError::Forbidden(format!(
                "{method} -> HTTP 403"
            )));
        }
        let preview: String = resp_text.chars().take(300).collect();
        Err(NotionUnofficialError::Permanent(format!(
            "{method}: HTTP {status} body={preview:?}"
        )))
    }

    pub async fn load_user_content(&self) -> Result<Value, NotionUnofficialError> {
        self.post("loadUserContent", &Value::Object(Default::default()))
            .await
    }

    pub async fn get_spaces(&self) -> Result<Value, NotionUnofficialError> {
        self.post("getSpaces", &Value::Object(Default::default()))
            .await
    }

    pub async fn get_notification_log(
        &self,
        space_id: &str,
        size: u32,
        cursor: Option<&Value>,
        type_: &str,
    ) -> Result<Value, NotionUnofficialError> {
        let mut body = serde_json::Map::new();
        body.insert("spaceId".into(), Value::String(space_id.into()));
        body.insert("size".into(), Value::from(size));
        body.insert("type".into(), Value::String(type_.into()));
        if let Some(c) = cursor {
            body.insert("cursor".into(), c.clone());
        }
        self.post("getNotificationLog", &Value::Object(body)).await
    }
}
