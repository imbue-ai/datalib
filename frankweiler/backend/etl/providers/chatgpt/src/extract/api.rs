//! ChatGPT API transport: shell out to `latchkey curl` per request.
//!
//! Mirrors `src/download/chatgpt_web.py:_curl_get` / `_get`. We pass
//! `-D -` so we get response headers (status + `Retry-After`) without
//! seeing latchkey-injected request headers, and `-o <tempfile>` so
//! the body never tangles with the header dump on stdout. Auth +
//! cookie injection are entirely latchkey's job; Cloudflare TLS
//! fingerprinting is `curl_impersonate`'s (the user is expected to
//! `export LATCHKEY_CURL=/path/to/curl_impersonate-chrome` before
//! invoking this binary).

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Context;
use serde_json::Value;
use tokio::process::Command;
use tokio::time::sleep;
use tracing::{instrument, warn};

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

    async fn curl_get(
        &mut self,
        url: &str,
    ) -> anyhow::Result<(u16, String, BTreeMap<String, String>)> {
        let body_file = tempfile::NamedTempFile::new().context("create body tempfile")?;
        let body_path = body_file.path().to_path_buf();
        let t0 = std::time::Instant::now();
        let proc = tokio::time::timeout(
            LATCHKEY_TIMEOUT,
            Command::new("latchkey")
                .args([
                    "curl",
                    "-sS",
                    "-D",
                    "-",
                    "-H",
                    "Accept: application/json",
                    "-o",
                ])
                .arg(&body_path)
                .arg(url)
                .output(),
        )
        .await
        .map_err(|_| anyhow::anyhow!("latchkey curl timed out: {url}"))?
        .context("latchkey curl spawn failed")?;
        self.network_seconds += t0.elapsed().as_secs_f64();
        self.requests += 1;

        if !proc.status.success() {
            let stderr = String::from_utf8_lossy(&proc.stderr);
            anyhow::bail!(
                "latchkey curl exit {}; stderr={:?}",
                proc.status.code().unwrap_or(-1),
                stderr.chars().take(300).collect::<String>()
            );
        }

        let header_dump = String::from_utf8_lossy(&proc.stdout).into_owned();
        let (status, headers) = parse_status_and_headers(&header_dump);
        let body = std::fs::read_to_string(&body_path).context("read body tempfile")?;
        Ok((status, body, headers))
    }

    #[instrument(skip(self), fields(path = path))]
    pub async fn get(&mut self, path: &str) -> Result<Value, ChatGPTError> {
        let url = format!("{BASE}{path}");
        let mut waited = Duration::from_secs(0);
        loop {
            let t0 = std::time::Instant::now();
            let (status, body, headers) = self
                .curl_get(&url)
                .await
                .map_err(|e| ChatGPTError::Permanent(format!("GET {path}: {e}")))?;
            if status == 200 {
                let value: Value = serde_json::from_str(&body).map_err(|e| {
                    let preview: String = body.chars().take(200).collect();
                    ChatGPTError::Permanent(format!(
                        "GET {path}: 200 but non-JSON body: {e}; body[:200]={preview:?}"
                    ))
                })?;
                let bytes = body.len() as u64;
                let duration_ms = t0.elapsed().as_millis() as u64;
                events::item_fetched(&url, bytes, duration_ms);
                return Ok(value);
            }
            if status == 429 {
                let wait = backoff_from_retry_after(headers.get("retry-after"), waited);
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
            return Err(ChatGPTError::Permanent(format!(
                "GET {path} -> HTTP {status} cf-mitigated={:?} body={:?}",
                headers.get("cf-mitigated"),
                body.chars().take(300).collect::<String>()
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

fn backoff_from_retry_after(retry_after: Option<&String>, waited: Duration) -> Duration {
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

/// Parse a `curl -D -` dump (status line + headers, possibly multiple
/// blocks if redirects were followed) into the *final* response's
/// `(status, lowercased-headers)`.
pub fn parse_status_and_headers(dump: &str) -> (u16, BTreeMap<String, String>) {
    let mut status: u16 = 0;
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    for block in dump.split("\r\n\r\n") {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }
        let mut lines = block.lines();
        let Some(first) = lines.next() else {
            continue;
        };
        if let Some(rest) = first.strip_prefix("HTTP/") {
            let mut parts = rest.split_whitespace();
            let _ver = parts.next();
            if let Some(code) = parts.next() {
                if let Ok(c) = code.parse::<u16>() {
                    status = c;
                    headers.clear();
                }
            }
        }
        for line in lines {
            if let Some((name, value)) = line.split_once(':') {
                headers.insert(name.trim().to_ascii_lowercase(), value.trim().to_string());
            }
        }
    }
    (status, headers)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_dump() {
        let dump = "HTTP/2 200 \r\ncontent-type: application/json\r\nretry-after: 7\r\n\r\n";
        let (s, h) = parse_status_and_headers(dump);
        assert_eq!(s, 200);
        assert_eq!(h.get("retry-after").map(String::as_str), Some("7"));
    }

    #[test]
    fn last_block_wins_on_redirect() {
        let dump = "HTTP/2 302 \r\nlocation: /x\r\n\r\nHTTP/2 200 \r\nx-foo: bar\r\n\r\n";
        let (s, h) = parse_status_and_headers(dump);
        assert_eq!(s, 200);
        assert!(!h.contains_key("location"));
        assert_eq!(h.get("x-foo").map(String::as_str), Some("bar"));
    }

    #[test]
    fn backoff_grows_with_waited() {
        let b0 = backoff_from_retry_after(None, Duration::from_secs(0));
        let b1 = backoff_from_retry_after(None, Duration::from_secs(15));
        assert!(b1 >= b0);
        assert!(b1 <= Duration::from_secs(60));
    }
}
