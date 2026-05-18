//! Anthropic (claude.ai) API transport. Shell out to `latchkey curl`
//! per request, mirroring `src/download/claude_web.py:_get`. Status
//! comes from `-w "%{http_code}"` (simpler than the ChatGPT side
//! since claude.ai doesn't 429 us in practice; if that ever changes
//! we'd grow a backoff loop like `chatgpt/extract/api.rs` has).

use std::time::Duration;

use anyhow::Context;
use serde_json::Value;
use tokio::process::Command;
use tracing::instrument;

use frankweiler_etl::obs::events;

pub const BASE: &str = "https://claude.ai/api";
pub const LATCHKEY_TIMEOUT: Duration = Duration::from_secs(120);

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
        let body_file = tempfile::NamedTempFile::new()
            .map_err(|e| ClaudeError::Permanent(format!("tempfile: {e}")))?;
        let body_path = body_file.path().to_path_buf();
        let t0 = std::time::Instant::now();
        let proc = tokio::time::timeout(
            LATCHKEY_TIMEOUT,
            Command::new("latchkey")
                .args(["curl", "-sS", "-H", "Accept: application/json", "-o"])
                .arg(&body_path)
                .arg("-w")
                .arg("%{http_code}")
                .arg(&url)
                .output(),
        )
        .await
        .map_err(|_| ClaudeError::Permanent(format!("GET {path}: latchkey curl timed out")))?
        .map_err(|e| ClaudeError::Permanent(format!("GET {path}: spawn failed: {e}")))?;
        self.network_seconds += t0.elapsed().as_secs_f64();
        self.requests += 1;

        if !proc.status.success() {
            let stderr = String::from_utf8_lossy(&proc.stderr);
            return Err(ClaudeError::Permanent(format!(
                "GET {path}: latchkey curl exit {}; stderr={:?}",
                proc.status.code().unwrap_or(-1),
                stderr.chars().take(300).collect::<String>()
            )));
        }
        let status_txt = String::from_utf8_lossy(&proc.stdout).trim().to_string();
        let status: u16 = status_txt.parse().unwrap_or(0);
        let body = std::fs::read_to_string(&body_path)
            .with_context(|| format!("read body for {path}"))
            .map_err(|e| ClaudeError::Permanent(e.to_string()))?;
        if status == 403 {
            return Err(ClaudeError::Forbidden(format!("GET {path} -> HTTP 403")));
        }
        if status != 200 {
            return Err(ClaudeError::Permanent(format!(
                "GET {path} -> HTTP {status}: {:?}",
                body.chars().take(300).collect::<String>()
            )));
        }
        let value: Value = serde_json::from_str(&body).map_err(|e| {
            let preview: String = body.chars().take(200).collect();
            ClaudeError::Permanent(format!(
                "GET {path}: invalid JSON: {e}; body[:200]={preview:?}"
            ))
        })?;
        events::item_fetched(&url, body.len() as u64, t0.elapsed().as_millis() as u64);
        Ok(value)
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
