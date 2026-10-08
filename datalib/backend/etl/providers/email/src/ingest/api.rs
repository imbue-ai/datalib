//! JMAP method-call transport.

use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::{json, Value};

use datalib_etl_web::http::{latchkey_curl, HttpError, HttpRequest, HttpService, LatchkeySettings};

use super::session::{Session, CAP_CORE, CAP_MAIL};

/// One JMAP method call. The CALL_ID is `"a"` for every single-method
/// envelope we send; we don't currently chain method calls via
/// back-references.
const CALL_ID: &str = "a";

/// Whether `dolt diff` should see the body bytes — JMAP method bodies
/// aren't huge, but pre-budgeting one MB is enough headroom for any
/// reasonable `Email/get` page including bodyValues.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// The request one JMAP method call goes out as. Public because a
/// playback fixture is keyed on its exact bytes, so a test that writes
/// one builds it here rather than re-spelling the envelope.
pub fn method_request(session: &Session, method: &str, args: Value) -> Result<HttpRequest> {
    let envelope = json!({
        "using": [CAP_CORE, CAP_MAIL],
        "methodCalls": [[method, args, CALL_ID]],
    });
    let body = serde_json::to_vec(&envelope).context("serialize JMAP envelope")?;
    Ok(
        HttpRequest::post_json(HttpService::Jmap, &session.api_url, body)
            .latchkey(session.latchkey.clone())
            .timeout(REQUEST_TIMEOUT),
    )
}

pub async fn call(session: &Session, method: &str, args: Value) -> Result<Value> {
    let req = method_request(session, method, args)?;
    let resp = latchkey_curl(&req).await.map_err(JmapError::Http)?;
    if !(200..300).contains(&resp.status) {
        return Err(JmapError::Status {
            what: method.to_string(),
            status: resp.status,
            body: resp.body_str().to_string(),
        }
        .into());
    }
    let answer = |detail: String| JmapError::Answer {
        method: method.to_string(),
        detail,
    };
    let body: Value = serde_json::from_slice(&resp.body)
        .map_err(|e| answer(format!("the response is not JSON: {e}")))?;
    let responses = body
        .get("methodResponses")
        .and_then(|v| v.as_array())
        .ok_or_else(|| answer(format!("no methodResponses in {body}")))?;
    let first = responses
        .first()
        .ok_or_else(|| answer("empty methodResponses".into()))?;
    let arr = first
        .as_array()
        .ok_or_else(|| answer("methodResponses[0] not an array".into()))?;
    let name = arr
        .first()
        .and_then(|v| v.as_str())
        .ok_or_else(|| answer("methodResponses[0][0] not a string".into()))?;
    let args = arr
        .get(1)
        .cloned()
        .ok_or_else(|| answer("methodResponses[0][1] missing".into()))?;
    if name == "error" {
        return Err(JmapError::Method {
            method: method.to_string(),
            kind: args
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string(),
            args,
        }
        .into());
    }
    Ok(args)
}

/// Everything a JMAP request can fail with, as opposed to the store a
/// caller writes the answer into: the run tells the two apart, because
/// an answer that did not come is a `problems` row and a write that did
/// not land fails the step.
#[derive(Debug, thiserror::Error)]
pub enum JmapError {
    #[error(transparent)]
    Http(HttpError),
    #[error("JMAP {what} → HTTP {status}: {body}")]
    Status {
        what: String,
        status: u16,
        body: String,
    },
    #[error("JMAP {method}: {detail}")]
    Answer { method: String, detail: String },
    /// The server answered the call with a method-level error; `kind`
    /// is its `type`.
    #[error("JMAP {method}: error: {args}")]
    Method {
        method: String,
        kind: String,
        args: Value,
    },
}

impl JmapError {
    /// Nothing after this will fare better: the retry loop gave up, or
    /// the server refused the credential.
    fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Http(HttpError::GaveUp { .. })
                | Self::Status {
                    status: 401 | 403,
                    ..
                }
        )
    }
}

/// The request failed upstream, rather than the store refusing a write.
pub fn is_upstream(e: &anyhow::Error) -> bool {
    e.downcast_ref::<JmapError>().is_some()
}

/// The server no longer keeps the changes since the state it was asked
/// about (RFC 8620 §5.2): only a new listing can say what it has now.
pub fn cannot_calculate_changes(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<JmapError>(),
        Some(JmapError::Method { kind, .. }) if kind == "cannotCalculateChanges"
    )
}

/// See [`JmapError::is_terminal`].
pub fn is_terminal(e: &anyhow::Error) -> bool {
    e.downcast_ref::<JmapError>()
        .is_some_and(JmapError::is_terminal)
}

pub async fn download_bytes(
    url: &str,
    timeout: Duration,
    latchkey: &LatchkeySettings,
) -> Result<(Vec<u8>, Option<String>)> {
    let req = HttpRequest::get(HttpService::Jmap, url)
        .latchkey(latchkey.clone())
        .timeout(timeout);
    let resp = latchkey_curl(&req).await.map_err(JmapError::Http)?;
    if !(200..300).contains(&resp.status) {
        return Err(JmapError::Status {
            what: format!("download {url}"),
            status: resp.status,
            body: String::new(),
        }
        .into());
    }
    let content_type = resp.header("content-type").map(str::to_string);
    Ok((resp.body, content_type))
}
