//! Notion official API client (`api.notion.com/v1`) via `latchkey curl`.
//! Latchkey injects the Bearer token + `Notion-Version` header for the
//! `notion` service; don't add them here or Notion 400s on duplicate.
//! Port of `NotionOfficialClient` in `src/download/notion_official.py`.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use serde_json::Value;
use tokio::process::Command;

use frankweiler_etl::obs::events;

pub const BASE: &str = "https://api.notion.com/v1";
pub const LATCHKEY_TIMEOUT: Duration = Duration::from_secs(180);
pub const PAGE_SIZE: u32 = 100;
const RETRY_MAX: u32 = 6;
const RETRY_INITIAL_BACKOFF_MS: u64 = 2_000;
const RETRY_MAX_BACKOFF_MS: u64 = 60_000;

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
}

impl Default for NotionOfficialClient {
    fn default() -> Self {
        Self {
            requests: AtomicU64::new(0),
            network_ms: AtomicU64::new(0),
        }
    }
}

impl NotionOfficialClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn request_count(&self) -> u64 {
        self.requests.load(Ordering::Relaxed)
    }

    pub fn network_seconds(&self) -> f64 {
        (self.network_ms.load(Ordering::Relaxed) as f64) / 1000.0
    }

    async fn request(
        &self,
        method: &str,
        path: &str,
        body: Option<&Value>,
    ) -> Result<Value, NotionOfficialError> {
        let url = format!("{BASE}{path}");
        let body_str = body.map(|b| b.to_string());

        let mut backoff_ms = RETRY_INITIAL_BACKOFF_MS;
        for attempt in 0..=RETRY_MAX {
            let mut cmd = Command::new("latchkey");
            cmd.args([
                "curl",
                "-sS",
                "-X",
                method,
                "-H",
                "Accept: application/json",
            ]);
            if let Some(ref payload) = body_str {
                cmd.args(["-H", "Content-Type: application/json", "--data"]);
                cmd.arg(payload);
            }
            cmd.args(["-w", "\n%{http_code}"]).arg(&url);

            let t0 = std::time::Instant::now();
            let proc = tokio::time::timeout(LATCHKEY_TIMEOUT, cmd.output())
                .await
                .map_err(|_| {
                    NotionOfficialError::Permanent(format!(
                        "{method} {path}: latchkey curl timed out"
                    ))
                })?
                .map_err(|e| {
                    NotionOfficialError::Permanent(format!("{method} {path}: spawn: {e}"))
                })?;
            let elapsed_ms = t0.elapsed().as_millis() as u64;
            self.network_ms.fetch_add(elapsed_ms, Ordering::Relaxed);
            self.requests.fetch_add(1, Ordering::Relaxed);

            if !proc.status.success() {
                return Err(NotionOfficialError::Permanent(format!(
                    "{method} {path}: latchkey exit {}; stderr={:?}",
                    proc.status.code().unwrap_or(-1),
                    String::from_utf8_lossy(&proc.stderr)
                        .chars()
                        .take(300)
                        .collect::<String>()
                )));
            }
            let stdout = String::from_utf8_lossy(&proc.stdout);
            let (body_text, status_txt) = match stdout.rfind('\n') {
                Some(nl) => (&stdout[..nl], stdout[nl + 1..].trim()),
                None => ("", stdout.trim()),
            };
            let status: u16 = status_txt.parse().unwrap_or(0);

            if status == 200 {
                let value: Value = serde_json::from_str(body_text).map_err(|e| {
                    let preview: String = body_text.chars().take(200).collect();
                    NotionOfficialError::Permanent(format!(
                        "{method} {path}: HTTP 200 but non-JSON: {e}; body[:200]={preview:?}"
                    ))
                })?;
                events::item_fetched(&url, body_text.len() as u64, elapsed_ms);
                return Ok(value);
            }
            if status == 403 {
                return Err(NotionOfficialError::Forbidden(format!(
                    "{method} {path} -> HTTP 403"
                )));
            }
            if matches!(status, 429 | 502 | 503 | 504) {
                if attempt == RETRY_MAX {
                    return Err(NotionOfficialError::Permanent(format!(
                        "{method} {path}: HTTP {status} after {attempt} retries"
                    )));
                }
                tracing::warn!(
                    method,
                    path,
                    status,
                    backoff_ms,
                    attempt = attempt + 1,
                    "transient; sleeping"
                );
                tokio::time::sleep(Duration::from_millis(backoff_ms)).await;
                backoff_ms = (backoff_ms * 2).min(RETRY_MAX_BACKOFF_MS);
                continue;
            }
            let preview: String = body_text.chars().take(300).collect();
            return Err(NotionOfficialError::Permanent(format!(
                "{method} {path}: HTTP {status} body={preview:?}"
            )));
        }
        unreachable!()
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

    pub async fn get_database(&self, database_id: &str) -> Result<Value, NotionOfficialError> {
        self.request("GET", &format!("/databases/{database_id}"), None)
            .await
    }

    pub async fn query_database(
        &self,
        database_id: &str,
        start_cursor: Option<&str>,
    ) -> Result<Value, NotionOfficialError> {
        let mut body = serde_json::Map::new();
        body.insert("page_size".into(), Value::from(PAGE_SIZE));
        if let Some(c) = start_cursor {
            body.insert("start_cursor".into(), Value::String(c.into()));
        }
        self.request(
            "POST",
            &format!("/databases/{database_id}/query"),
            Some(&Value::Object(body)),
        )
        .await
    }
}
