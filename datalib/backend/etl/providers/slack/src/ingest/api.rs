//! Slack API transport: latchkey curl shellout with retry.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;
use serde_json::Value;
use tracing::instrument;

use datalib_etl::events;
use datalib_etl_web::http::{
    default_retryability, latchkey_curl_classified, parse_retry_after, HttpError, HttpRequest,
    HttpResponse, HttpService, LatchkeySettings, Retryability,
};

pub const LATCHKEY_TIMEOUT: Duration = Duration::from_secs(60);
pub const LATCHKEY_FILE_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(thiserror::Error, Debug)]
pub enum SlackError {
    #[error("{0}")]
    Permanent(String),
    /// Slack answered, and said this credential may not call `method`:
    /// the token is the wrong kind, lacks the scope, or the method does
    /// not exist for it. Asking again will not help; a different token
    /// would.
    #[error("{method}: ok=false error={error:?}")]
    Refused { method: String, error: String },
    /// The run was told to stop, so the transport sent nothing. Not a
    /// failure of what was asked for.
    #[error("{0}")]
    Interrupted(String),
}

const REFUSAL_CODES: &[&str] = &["not_allowed_token_type", "missing_scope", "unknown_method"];

/// Slack-specific retry classifier. Slack signals a rate limit either as a
/// plain HTTP 429 (newer Web API tiers — covered by the default classifier)
/// or, on older methods, as **HTTP 200** with
/// `{"ok":false,"error":"ratelimited"}` in the body. The status-code
/// chokepoint can't see the latter, so detect it here and surface it as
/// retryable; the shared loop then honors any `Retry-After` header and the
/// orchestrator's give-up bounds.
fn slack_retryability(resp: &HttpResponse) -> Retryability {
    if resp.status == 200 && resp.body_str().contains("\"error\":\"ratelimited\"") {
        return Retryability::Retry {
            retry_after: parse_retry_after(resp.header("retry-after")),
        };
    }
    default_retryability(resp)
}

/// Successful Slack API call plus wall-clock duration of the underlying
/// HTTP exchange.
pub struct SlackCall {
    pub response: Value,
    pub duration_ms: u64,
}

fn url_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

pub(crate) fn build_url(method: &str, params: &BTreeMap<String, String>) -> String {
    let base = format!("https://slack.com/api/{}", method);
    if params.is_empty() {
        return base;
    }
    let qs: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
        .collect();
    format!("{}?{}", base, qs.join("&"))
}

async fn call_slack_once(
    method: &str,
    params: &BTreeMap<String, String>,
    latchkey: &LatchkeySettings,
) -> Result<Value, SlackError> {
    let url = build_url(method, params);
    let req = HttpRequest::get(HttpService::Slack, &url)
        .latchkey(latchkey.clone())
        .timeout(LATCHKEY_TIMEOUT);
    // Rate-limit (429 + the HTTP-200 `ratelimited` body) and transient
    // retry is handled centrally in the shared chokepoint via
    // `slack_retryability`; a terminal error here (incl. `GaveUp` after the
    // guard tripped) is mapped straight to `Permanent`.
    let resp = latchkey_curl_classified(&req, slack_retryability)
        .await
        .map_err(|e: HttpError| match e {
            HttpError::PlaybackMiss(msg) => SlackError::Permanent(format!("{method}: {msg}")),
            HttpError::Interrupted { .. } => SlackError::Interrupted(format!("{method}: {e}")),
            _ => SlackError::Permanent(format!("{method}: {e}")),
        })?;

    if resp.status != 200 {
        return Err(SlackError::Permanent(format!(
            "{method}: HTTP {} body={:?}",
            resp.status,
            resp.body_str().chars().take(200).collect::<String>()
        )));
    }
    let body = resp.body_str();
    let data: Value = serde_json::from_str(&body).map_err(|e| {
        let preview: String = body.chars().take(200).collect();
        SlackError::Permanent(format!("{}: invalid JSON: {:?} ({})", method, preview, e))
    })?;
    let ok = data.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    if !ok {
        // `ratelimited` is retried by the chokepoint, so reaching here with
        // `ok:false` means a genuine API error (or the guard gave up — which
        // surfaces as `GaveUp` → `Permanent` above, never as a parsed body).
        let err = data
            .get("error")
            .and_then(|v| v.as_str())
            .unwrap_or("unknown");
        if REFUSAL_CODES.contains(&err) {
            return Err(SlackError::Refused {
                method: method.to_string(),
                error: err.to_string(),
            });
        }
        return Err(SlackError::Permanent(format!(
            "{}: ok=false error={:?}",
            method, err
        )));
    }
    Ok(data)
}

#[instrument(skip(params, latchkey), fields(method = method))]
pub async fn call_slack(
    method: &str,
    params: &BTreeMap<String, String>,
    latchkey: &LatchkeySettings,
) -> Result<SlackCall, SlackError> {
    let t0 = std::time::Instant::now();
    let response = call_slack_once(method, params, latchkey).await?;
    let duration_ms = t0.elapsed().as_millis() as u64;
    let bytes = response.to_string().len() as u64;
    events::item_fetched(&format!("slack.api/{}", method), bytes, duration_ms);
    Ok(SlackCall {
        response,
        duration_ms,
    })
}

/// Where Slack serves a file object's bytes. `None` for a tombstone, a
/// file hosted elsewhere, or one with no id: nothing to fetch, so no edge.
/// The URL carries no signature (the session credential signs the
/// request), so the one in a stored message stays good.
pub(crate) fn served(file_obj: &Value) -> Option<(&str, &str)> {
    let id = file_obj.get("id").and_then(Value::as_str)?;
    if file_obj.get("mode").and_then(Value::as_str) == Some("tombstone")
        || file_obj.get("is_external").and_then(Value::as_bool) == Some(true)
    {
        return None;
    }
    let url = file_obj
        .get("url_private_download")
        .and_then(Value::as_str)
        .or_else(|| file_obj.get("url_private").and_then(Value::as_str))?;
    Some((id, url))
}

pub fn served_file_id(file_obj: &Value) -> Option<&str> {
    served(file_obj).map(|(id, _)| id)
}
