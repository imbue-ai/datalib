//! Slack API transport: latchkey curl shellout with retry.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;
use serde_json::Value;
use tracing::instrument;

use datalib_etl::upstream::Upstream;
use datalib_etl_web::http::{
    default_retryability, latchkey_curl_classified, parse_retry_after, HttpError, HttpRequest,
    HttpResponse, HttpService, LatchkeySettings, Retryability,
};

pub const LATCHKEY_TIMEOUT: Duration = Duration::from_secs(60);
pub const LATCHKEY_FILE_TIMEOUT: Duration = Duration::from_secs(600);

#[derive(thiserror::Error, Debug)]
pub enum SlackError {
    #[error("{0}")]
    Permanent(String, #[source] Option<Upstream>),
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

/// Slack's `error` codes for a token it will not take at all.
const SIGNED_OUT_CODES: &[&str] = &[
    "not_authed",
    "invalid_auth",
    "account_inactive",
    "token_revoked",
    "token_expired",
];

/// Slack's `error` codes for a call that may succeed if sent again.
/// `ratelimited` is Slack's older form of a 429. `internal_error` is
/// "likely due to a transient issue on our end", and Slack recommends
/// retrying it; `service_unavailable` is "temporarily unavailable".
/// `fatal_error` and `request_timeout` (a POST body that arrived
/// truncated) are not here: Slack calls neither transient.
const TRANSIENT_CODES: &[&str] = &["ratelimited", "internal_error", "service_unavailable"];

/// The shared retry loop sees only the status, and Slack answers these
/// errors with HTTP 200 and `{"ok":false,"error":...}`, so a 200 is read
/// for its `error` here.
fn slack_retryability(resp: &HttpResponse) -> Retryability {
    if resp.status == 200 && names_transient_error(&resp.body) {
        return Retryability::Retry {
            retry_after: parse_retry_after(resp.header("retry-after")),
        };
    }
    default_retryability(resp)
}

fn names_transient_error(body: &[u8]) -> bool {
    #[derive(serde::Deserialize)]
    struct Status {
        #[serde(default)]
        ok: bool,
        error: Option<String>,
    }
    match serde_json::from_slice::<Status>(body) {
        Ok(Status {
            ok: false,
            error: Some(error),
        }) => TRANSIENT_CODES.contains(&error.as_str()),
        _ => false,
    }
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
    // A 429, a 502 to 504 and a 200 naming one of `TRANSIENT_CODES` are
    // retried by the shared chokepoint via `slack_retryability`; a terminal
    // error here (incl. `GaveUp` after the guard tripped) is mapped
    // straight to `Permanent`.
    let resp = latchkey_curl_classified(&req, slack_retryability)
        .await
        .map_err(|e: HttpError| match e {
            HttpError::PlaybackMiss(msg) => SlackError::Permanent(format!("{method}: {msg}"), None),
            HttpError::Interrupted { .. } => SlackError::Interrupted(format!("{method}: {e}")),
            _ => SlackError::Permanent(format!("{method}: {e}"), e.upstream()),
        })?;

    if resp.status != 200 {
        return Err(SlackError::Permanent(
            format!(
                "{method}: HTTP {} body={:?}",
                resp.status,
                resp.body_str().chars().take(200).collect::<String>()
            ),
            Upstream::of_status(resp.status),
        ));
    }
    let body = resp.body_str();
    let data: Value = serde_json::from_str(&body).map_err(|e| {
        let preview: String = body.chars().take(200).collect();
        SlackError::Permanent(
            format!("{}: invalid JSON: {:?} ({})", method, preview, e),
            None,
        )
    })?;
    let ok = data.get("ok").and_then(|v| v.as_bool()).unwrap_or(false);
    if !ok {
        // `TRANSIENT_CODES` are retried by the chokepoint until it gives up,
        // which surfaces as `GaveUp` → `Permanent` above, so reaching here
        // with `ok:false` means an error that asking again will not fix.
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
        let signed_out = SIGNED_OUT_CODES
            .contains(&err)
            .then_some(Upstream::CredentialRefused);
        return Err(SlackError::Permanent(
            format!("{}: ok=false error={:?}", method, err),
            signed_out,
        ));
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

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(status: u16, body: &str, retry_after: Option<&str>) -> HttpResponse {
        HttpResponse {
            status,
            headers: retry_after
                .map(|v| ("retry-after".to_string(), v.to_string()))
                .into_iter()
                .collect(),
            body: body.as_bytes().to_vec(),
            duration_ms: 0,
        }
    }

    const NO_WAIT: Retryability = Retryability::Retry { retry_after: None };

    /// `internal_error` was taken as a permanent failure, so a thread
    /// Slack failed once was missing until the next sync.
    #[test]
    fn a_transient_slack_error_is_retried() {
        for body in [
            r#"{"ok":false,"error":"internal_error"}"#,
            r#"{"ok": false, "error": "internal_error"}"#,
            r#"{"ok":false,"error":"service_unavailable"}"#,
            r#"{"ok":false,"error":"ratelimited"}"#,
        ] {
            assert_eq!(
                slack_retryability(&answer(200, body, None)),
                NO_WAIT,
                "{body}"
            );
        }
        assert_eq!(
            slack_retryability(&answer(
                200,
                r#"{"ok": false, "error": "ratelimited"}"#,
                Some("7")
            )),
            Retryability::Retry {
                retry_after: Some(Duration::from_secs(7))
            }
        );
    }

    #[test]
    fn any_other_answer_is_handed_back() {
        for body in [
            r#"{"ok":true,"messages":[]}"#,
            r#"{"ok":true,"error":"internal_error"}"#,
            r#"{"ok":false,"error":"fatal_error"}"#,
            r#"{"ok":false,"error":"request_timeout"}"#,
            r#"{"ok":false,"error":"thread_not_found"}"#,
            r#"{"ok":false,"error":"invalid_auth"}"#,
            r#"{"ok":false}"#,
            r#"{"ok":true,"messages":[{"text":"\"error\":\"internal_error\""}]}"#,
            "not json",
        ] {
            assert_eq!(
                slack_retryability(&answer(200, body, None)),
                Retryability::Accept,
                "{body}"
            );
        }
        assert_eq!(slack_retryability(&answer(503, "", None)), NO_WAIT);
        assert_eq!(
            slack_retryability(&answer(500, "", None)),
            Retryability::Accept
        );
    }
}
