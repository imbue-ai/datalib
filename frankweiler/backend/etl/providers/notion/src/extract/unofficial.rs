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
use tokio::process::Command;

use frankweiler_etl::obs::events;

pub const BASE: &str = "https://www.notion.so/api/v3";
pub const LATCHKEY_TIMEOUT: Duration = Duration::from_secs(180);
const RETRY_MAX: u32 = 6;
const RETRY_INITIAL_BACKOFF_MS: u64 = 2_000;
const RETRY_MAX_BACKOFF_MS: u64 = 60_000;

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
        let payload = body.to_string();
        let mut backoff_ms = RETRY_INITIAL_BACKOFF_MS;
        for attempt in 0..=RETRY_MAX {
            let body_file = tempfile::Builder::new()
                .prefix("notion-uo-")
                .suffix(".json")
                .tempfile()
                .map_err(|e| NotionUnofficialError::Permanent(format!("tempfile: {e}")))?;
            let body_path = body_file.path().to_path_buf();
            let t0 = std::time::Instant::now();
            let proc = tokio::time::timeout(
                LATCHKEY_TIMEOUT,
                Command::new("latchkey")
                    .args([
                        "curl",
                        "-sS",
                        "-X",
                        "POST",
                        "-H",
                        "Content-Type: application/json",
                        "-H",
                        "Accept: application/json",
                        "--data",
                    ])
                    .arg(&payload)
                    .arg("-o")
                    .arg(&body_path)
                    .args(["-w", "%{http_code}"])
                    .arg(&url)
                    .output(),
            )
            .await
            .map_err(|_| {
                NotionUnofficialError::Permanent(format!("{method}: latchkey curl timed out"))
            })?
            .map_err(|e| NotionUnofficialError::Permanent(format!("{method}: spawn: {e}")))?;
            let elapsed_ms = t0.elapsed().as_millis() as u64;
            self.network_ms.fetch_add(elapsed_ms, Ordering::Relaxed);
            self.requests.fetch_add(1, Ordering::Relaxed);

            if !proc.status.success() {
                return Err(NotionUnofficialError::Permanent(format!(
                    "{method}: latchkey exit {}; stderr={:?}",
                    proc.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&proc.stderr)
                        .chars()
                        .take(300)
                        .collect::<String>()
                )));
            }
            let status_txt = String::from_utf8_lossy(&proc.stdout).trim().to_string();
            let status: u16 = status_txt.parse().unwrap_or(0);
            let resp_text = std::fs::read_to_string(&body_path).unwrap_or_default();

            if status == 200 {
                let value: Value = serde_json::from_str(&resp_text).map_err(|e| {
                    let preview: String = resp_text.chars().take(200).collect();
                    NotionUnofficialError::Permanent(format!(
                        "{method}: HTTP 200 but non-JSON: {e}; body[:200]={preview:?}"
                    ))
                })?;
                events::item_fetched(&url, resp_text.len() as u64, elapsed_ms);
                return Ok(value);
            }
            if status == 403 {
                return Err(NotionUnofficialError::Forbidden(format!(
                    "{method} -> HTTP 403"
                )));
            }
            if matches!(status, 429 | 502 | 503 | 504) {
                if attempt == RETRY_MAX {
                    return Err(NotionUnofficialError::Permanent(format!(
                        "{method}: HTTP {status} after {attempt} retries"
                    )));
                }
                tracing::warn!(method, status, backoff_ms, attempt = attempt + 1, "transient; sleeping");
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(RETRY_MAX_BACKOFF_MS);
                continue;
            }
            let preview: String = resp_text.chars().take(300).collect();
            return Err(NotionUnofficialError::Permanent(format!(
                "{method}: HTTP {status} body={preview:?}"
            )));
        }
        unreachable!()
    }

    pub async fn load_user_content(&self) -> Result<Value, NotionUnofficialError> {
        self.post("loadUserContent", &Value::Object(Default::default())).await
    }

    pub async fn get_spaces(&self) -> Result<Value, NotionUnofficialError> {
        self.post("getSpaces", &Value::Object(Default::default())).await
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
