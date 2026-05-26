//! Anthropic (claude.ai) API transport. Every request goes through
//! [`frankweiler_etl::http::latchkey_curl`], which captures the full
//! response (status + every header + body) and supports playback from
//! disk fixtures. Mirrors `src/download/claude_web.py:_get`.

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::Value;
use tracing::{debug, instrument, warn};

use frankweiler_etl::blobs::safe_filename;
use frankweiler_etl::http::{latchkey_curl, HttpError, HttpRequest};
use frankweiler_etl::latchkey::latchkey_tokio_command;
use frankweiler_etl::obs::events;

pub const BASE: &str = "https://claude.ai/api";
pub const LATCHKEY_TIMEOUT: Duration = Duration::from_secs(120);
pub const LATCHKEY_FILE_TIMEOUT: Duration = Duration::from_secs(600);
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

    /// `GET /api/account` — current authenticated user. Returns the
    /// raw object so the caller can keep whatever fields it cares about
    /// (`uuid`, `email_address`, `full_name`, plus a `memberships` block
    /// we ignore).
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

// ---------------------------------------------------------------------------
// File-download path. `preview_url` on a message's `files[]` entry is a
// relative `/api/<...>/preview` that returns image bytes directly (no
// signed-URL indirection). latchkey covers `claude.ai` URLs, so a single
// shellout suffices.
// ---------------------------------------------------------------------------

/// Download one Anthropic attachment into
/// `<media_dir>/<file_uuid>/<safe(file_name)>`. Returns
/// `downloaded` / `skipped` / `error`. The caller passes the `files[]`
/// entry as-is.
pub async fn download_one_file(file_obj: &Value, media_dir: &Path) -> Result<&'static str> {
    let Some(file_uuid) = file_obj.get("file_uuid").and_then(|v| v.as_str()) else {
        return Ok("error");
    };
    // `files[].preview_url` is set for images. For documents (PDFs etc.),
    // claude.ai exposes the original bytes via `document_asset.url`. Fall
    // back to that so PDFs come through too.
    let preview_path = file_obj
        .get("preview_url")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .or_else(|| {
            file_obj
                .get("document_asset")
                .and_then(|d| d.get("url"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
        });
    let preview_path = match preview_path {
        Some(p) => p,
        None => {
            warn!(
                event = "anthropic_media_no_preview_url",
                file_uuid = file_uuid,
            );
            return Ok("error");
        }
    };
    let url = if preview_path.starts_with("http") {
        preview_path.to_string()
    } else {
        format!("{CLAUDE_ORIGIN}{preview_path}")
    };
    let name = file_obj.get("file_name").and_then(|v| v.as_str());
    let safe = safe_filename(name, file_uuid);
    let target_dir = media_dir.join(file_uuid);
    let target = target_dir.join(&safe);
    if let Ok(meta) = std::fs::metadata(&target) {
        if meta.len() > 0 {
            return Ok("skipped");
        }
    }
    std::fs::create_dir_all(&target_dir)
        .with_context(|| format!("mkdir {}", target_dir.display()))?;

    let mut cmd = latchkey_tokio_command();
    cmd.arg("curl").arg("-fSL").arg("-o").arg(&target).arg(&url);
    let proc = tokio::time::timeout(LATCHKEY_FILE_TIMEOUT, cmd.output())
        .await
        .context("file curl timed out")?
        .context("file curl spawn failed")?;
    if !proc.status.success() {
        let _ = std::fs::remove_file(&target);
        let stderr_full = String::from_utf8_lossy(&proc.stderr).into_owned();
        let tail: String = stderr_full
            .chars()
            .rev()
            .take(200)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        warn!(
            event = "anthropic_media_failed",
            file_uuid = file_uuid,
            name = %safe,
            exit = proc.status.code().unwrap_or(-1),
            stderr = %tail.trim(),
        );
        return Ok("error");
    }
    let bytes = std::fs::metadata(&target).map(|m| m.len()).unwrap_or(0);
    events::item_fetched(&url, bytes, 0);
    debug!(
        event = "anthropic_media_downloaded",
        file_uuid = file_uuid,
        bytes = bytes
    );
    Ok("downloaded")
}

/// Walk every message in a conversation tree and download its `files[]`.
/// Dedupes by `file_uuid`.
pub async fn download_files_for_conversation(
    conv: &Value,
    media_dir: &Path,
) -> Result<BTreeMap<String, usize>> {
    let mut counts: BTreeMap<String, usize> = BTreeMap::new();
    for k in ["downloaded", "skipped", "error"] {
        counts.insert(k.to_string(), 0);
    }
    let messages = match conv.get("chat_messages").and_then(|v| v.as_array()) {
        Some(arr) => arr,
        None => return Ok(counts),
    };
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut targets: Vec<Value> = Vec::new();
    for msg in messages {
        if let Some(files) = msg.get("files").and_then(|v| v.as_array()) {
            for f in files {
                if let Some(id) = f.get("file_uuid").and_then(|v| v.as_str()) {
                    if seen.insert(id.to_string()) {
                        targets.push(f.clone());
                    }
                }
            }
        }
    }
    for f in &targets {
        let outcome = download_one_file(f, media_dir).await?;
        *counts.entry(outcome.to_string()).or_insert(0) += 1;
    }
    debug!(
        event = "anthropic_media_summary",
        downloaded = counts.get("downloaded").copied().unwrap_or(0),
        skipped = counts.get("skipped").copied().unwrap_or(0),
        errors = counts.get("error").copied().unwrap_or(0),
    );
    Ok(counts)
}
