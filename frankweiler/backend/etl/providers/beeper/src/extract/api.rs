//! Matrix Client-Server transport for `matrix.beeper.com`, threaded
//! through the shared `latchkey curl` shellout in
//! [`frankweiler_etl::http`].
//!
//! Latchkey is responsible for injecting the
//! `Authorization: Bearer <access_token>` header for the `beeper`
//! provider — the host (`matrix.beeper.com`) sits inside latchkey's
//! `beeper` service `baseApiUrls`. Don't add Authorization here.

use std::collections::BTreeMap;
use std::time::Duration;

use serde_json::Value;
use tracing::{debug, warn};

use frankweiler_etl::http::{latchkey_curl, HttpError, HttpRequest};

pub const PROVIDER: &str = "beeper";
pub const BASE: &str = "https://matrix.beeper.com";
pub const LATCHKEY_TIMEOUT: Duration = Duration::from_secs(60);
/// Larger budget for the initial `/sync?full_state=true` response.
/// On a 200-room account this is ~tens of MB; latchkey-curl streams
/// fine but the homeserver itself can take a while to assemble the
/// snapshot.
pub const LATCHKEY_SYNC_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(thiserror::Error, Debug)]
pub enum MatrixError {
    #[error("transient: {0}")]
    Transient(String),
    #[error("{0}")]
    Permanent(String),
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

fn build_url(path: &str, params: &BTreeMap<String, String>) -> String {
    let base = format!("{}{}", BASE, path);
    if params.is_empty() {
        return base;
    }
    let qs: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{}={}", url_encode(k), url_encode(v)))
        .collect();
    format!("{}?{}", base, qs.join("&"))
}

/// Single GET against the Matrix Client-Server API. The default
/// timeout suits short responses (`whoami`, `joined_rooms`, individual
/// `/state` calls); use [`matrix_get_with_timeout`] when you know the
/// response will be large — notably `/sync?full_state=true`.
///
/// No retry loop yet: transient curl failures bubble up as
/// `Transient` so a future retry wrapper can introspect them. The
/// status-code policy matches what Matrix actually emits: 429 is
/// transient, the rest of the non-2xx codes are permanent and surface
/// the body for the user.
pub async fn matrix_get(
    path: &str,
    params: &BTreeMap<String, String>,
) -> Result<Value, MatrixError> {
    matrix_get_with_timeout(path, params, LATCHKEY_TIMEOUT).await
}

/// Like [`matrix_get`], but with an explicit timeout. The big-response
/// callsites (`/sync`) pass [`LATCHKEY_SYNC_TIMEOUT`].
pub async fn matrix_get_with_timeout(
    path: &str,
    params: &BTreeMap<String, String>,
    timeout: Duration,
) -> Result<Value, MatrixError> {
    let url = build_url(path, params);
    debug!(event = "beeper_get", url = %url);
    let req = HttpRequest::get(PROVIDER, &url).timeout(timeout);
    let resp = latchkey_curl(&req).await.map_err(|e: HttpError| match e {
        HttpError::Timeout { .. } => MatrixError::Transient(format!("{path}: {e}")),
        HttpError::Curl {
            exit: 7 | 28 | 35 | 56,
            ..
        } => MatrixError::Transient(format!("{path}: {e}")),
        HttpError::PlaybackMiss(msg) => MatrixError::Permanent(format!("{path}: {msg}")),
        _ => MatrixError::Permanent(format!("{path}: {e}")),
    })?;

    if resp.status == 429 {
        return Err(MatrixError::Transient(format!(
            "{path}: HTTP 429 body={:?}",
            resp.body_str().chars().take(200).collect::<String>()
        )));
    }
    if resp.status < 200 || resp.status >= 300 {
        let body_preview: String = resp.body_str().chars().take(200).collect();
        if resp.status == 401 || resp.status == 403 {
            warn!(
                event = "beeper_auth_failed",
                status = resp.status,
                body = %body_preview
            );
        }
        return Err(MatrixError::Permanent(format!(
            "{path}: HTTP {} body={:?}",
            resp.status, body_preview
        )));
    }
    let body = resp.body_str();
    serde_json::from_str(&body).map_err(|e| {
        let preview: String = body.chars().take(200).collect();
        MatrixError::Permanent(format!(
            "{path}: HTTP {} but non-JSON: {e}; body[:200]={preview:?}",
            resp.status
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_url_no_params() {
        let u = build_url("/_matrix/client/v3/joined_rooms", &BTreeMap::new());
        assert_eq!(u, "https://matrix.beeper.com/_matrix/client/v3/joined_rooms");
    }

    #[test]
    fn build_url_with_params() {
        let mut p = BTreeMap::new();
        p.insert("dir".into(), "b".into());
        p.insert("limit".into(), "200".into());
        let u = build_url("/_matrix/client/v3/rooms/foo/messages", &p);
        assert!(u.contains("dir=b"));
        assert!(u.contains("limit=200"));
    }
}
