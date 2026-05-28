//! Anthropic (claude.ai) API transport. Every request goes through
//! [`frankweiler_etl::http::latchkey_curl`], which captures the full
//! response (status + every header + body) and supports playback from
//! disk fixtures. Mirrors `src/download/claude_web.py:_get`.
//!
//! Blob downloads live in `extract::mod` against the doltlite `blobs`
//! table; this module is transport-only.

use std::time::Duration;

use serde_json::Value;
use tracing::instrument;

use frankweiler_etl::events;
use frankweiler_etl::http::{latchkey_curl, HttpError, HttpRequest};

pub const BASE: &str = "https://claude.ai/api";
pub const LATCHKEY_TIMEOUT: Duration = Duration::from_secs(120);
pub const CLAUDE_ORIGIN: &str = "https://claude.ai";

#[derive(thiserror::Error, Debug)]
pub enum ClaudeError {
    #[error("forbidden: {0}")]
    Forbidden(String),
    #[error("{0}")]
    Permanent(String),
}

pub struct ClaudeClient {
    pub requests: u64,
    pub network_seconds: f64,
}

impl Default for ClaudeClient {
    fn default() -> Self {
        Self {
            requests: 0,
            network_seconds: 0.0,
        }
    }
}

impl ClaudeClient {
    pub fn new() -> Self {
        Self::default()
    }

    #[instrument(skip(self), fields(path = path))]
    pub async fn get(&mut self, path: &str) -> Result<Value, ClaudeError> {
        let url = format!("{BASE}{path}");
        let req = HttpRequest::get("anthropic", &url)
            .header("Accept", "application/json")
            .timeout(LATCHKEY_TIMEOUT);
        let resp = latchkey_curl(&req).await.map_err(map_transport_error)?;
        self.network_seconds += (resp.duration_ms as f64) / 1000.0;
        self.requests += 1;

        let body = resp.body_str();
        if resp.status == 403 {
            return Err(ClaudeError::Forbidden(format!("GET {path} -> HTTP 403")));
        }
        if resp.status != 200 {
            return Err(ClaudeError::Permanent(format!(
                "GET {path} -> HTTP {}: {:?}",
                resp.status,
                body.chars().take(300).collect::<String>()
            )));
        }
        let value: Value = serde_json::from_str(&body).map_err(|e| {
            let preview: String = body.chars().take(200).collect();
            ClaudeError::Permanent(format!(
                "GET {path}: invalid JSON: {e}; body[:200]={preview:?}"
            ))
        })?;
        events::item_fetched(&url, resp.body.len() as u64, resp.duration_ms);
        Ok(value)
    }

    /// `GET /api/account` — current authenticated user.
    pub async fn current_account(&mut self) -> Result<Value, ClaudeError> {
        self.get("/account").await
    }

    pub async fn list_orgs(&mut self) -> Result<Vec<Value>, ClaudeError> {
        let v = self.get("/organizations").await?;
        v.as_array()
            .cloned()
            .ok_or_else(|| ClaudeError::Permanent("/organizations: expected array".to_string()))
    }

    pub async fn list_conversations(&mut self, org_uuid: &str) -> Result<Vec<Value>, ClaudeError> {
        let v = self
            .get(&format!("/organizations/{org_uuid}/chat_conversations"))
            .await?;
        v.as_array().cloned().ok_or_else(|| {
            ClaudeError::Permanent(format!(
                "/organizations/{org_uuid}/chat_conversations: expected array"
            ))
        })
    }

    pub async fn get_conversation(
        &mut self,
        org_uuid: &str,
        conv_uuid: &str,
    ) -> Result<Value, ClaudeError> {
        let q = "tree=True&rendering_mode=messages&render_all_tools=true&consistency=strong";
        self.get(&format!(
            "/organizations/{org_uuid}/chat_conversations/{conv_uuid}?{q}"
        ))
        .await
    }
}

fn map_transport_error(e: HttpError) -> ClaudeError {
    ClaudeError::Permanent(e.to_string())
}
