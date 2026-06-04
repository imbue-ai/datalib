//! JMAP method-call transport.
//!
//! Wraps [`frankweiler_etl::http::latchkey_curl`] with JMAP-specific
//! request encoding and response unpacking. Every call sends a single
//! `{using, methodCalls}` envelope (RFC 8620 §3.2) and returns the
//! first method response's args.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

use frankweiler_etl::http::{latchkey_curl, HttpError, HttpRequest};

use super::session::{Session, CAP_CORE, CAP_MAIL};

/// One JMAP method call. The CALL_ID is `"a"` for every single-method
/// envelope we send; we don't currently chain method calls via
/// back-references.
const CALL_ID: &str = "a";

/// Whether `dolt diff` should see the body bytes — JMAP method bodies
/// aren't huge, but pre-budgeting one MB is enough headroom for any
/// reasonable `Email/get` page including bodyValues.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);

/// POST `{using, methodCalls: [[method, args, "a"]]}` and return the
/// arg object from the matching response. Surfaces `error` method
/// responses (per JMAP §3.6) as `Err`.
pub async fn call(session: &Session, method: &str, args: Value) -> Result<Value> {
    let envelope = json!({
        "using": [CAP_CORE, CAP_MAIL],
        "methodCalls": [[method, args, CALL_ID]],
    });
    let body = serde_json::to_vec(&envelope).context("serialize JMAP envelope")?;
    let req = HttpRequest::post_json("jmap", &session.api_url, body).timeout(REQUEST_TIMEOUT);
    let resp = latchkey_curl(&req).await.map_err(map_http_err)?;
    if !(200..300).contains(&resp.status) {
        return Err(anyhow!(
            "JMAP {method} → HTTP {}: {}",
            resp.status,
            resp.body_str(),
        ));
    }
    let body: Value = serde_json::from_slice(&resp.body)
        .with_context(|| format!("parse JMAP {method} response"))?;
    let responses = body
        .get("methodResponses")
        .and_then(|v| v.as_array())
        .ok_or_else(|| anyhow!("JMAP {method}: no methodResponses in {}", body))?;
    let first = responses
        .first()
        .ok_or_else(|| anyhow!("JMAP {method}: empty methodResponses"))?;
    let arr = first
        .as_array()
        .ok_or_else(|| anyhow!("JMAP {method}: methodResponses[0] not an array"))?;
    let name = arr
        .first()
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("JMAP {method}: methodResponses[0][0] not a string"))?;
    let args = arr
        .get(1)
        .cloned()
        .ok_or_else(|| anyhow!("JMAP {method}: methodResponses[0][1] missing"))?;
    if name == "error" {
        return Err(anyhow!("JMAP {method} error: {args}"));
    }
    Ok(args)
}

fn map_http_err(e: HttpError) -> anyhow::Error {
    anyhow!("{e}")
}

/// GET an arbitrary URL (e.g. `session.downloadUrl` after substitution)
/// and return the body bytes. Errors carry the HTTP status so callers
/// can record a transport-level failure on the bookkeeping sidecar.
pub async fn download_bytes(url: &str, timeout: Duration) -> Result<(Vec<u8>, Option<String>)> {
    let req = HttpRequest::get("jmap", url).timeout(timeout);
    let resp = latchkey_curl(&req).await.map_err(map_http_err)?;
    if !(200..300).contains(&resp.status) {
        return Err(anyhow!(
            "JMAP download {url} → HTTP {}",
            resp.status,
        ));
    }
    let content_type = resp.header("content-type").map(str::to_string);
    Ok((resp.body, content_type))
}
