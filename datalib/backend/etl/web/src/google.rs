//! What every download over a Google API shares: one JSON request, what
//! its error replies mean, and which links may go out without the
//! credential.

use anyhow::{Context, Result};
use datalib_etl::upstream::Upstream;
use serde_json::Value;

use crate::http::{
    default_retryability, latchkey_curl_classified, HttpRequest, HttpResponse, HttpService,
    LatchkeySettings, Retryability, ShownUrl,
};

/// One Google API, by the two names a request needs.
#[derive(Debug, Clone, Copy)]
pub struct GoogleApi {
    pub service: HttpService,
    /// The latchkey service a person signs in to.
    pub latchkey_service: &'static str,
}

/// Google's reply to a request it did not answer.
#[derive(Debug, thiserror::Error)]
#[error("GET {url}: {status}: {message}")]
pub struct ApiError {
    pub status: u16,
    /// Google's word for it (`EXPIRED_SYNC_TOKEN`, `fullSyncRequired`).
    pub reason: Option<String>,
    message: String,
    url: ShownUrl,
    #[source]
    means: Option<Upstream>,
}

impl ApiError {
    /// The first one in `e`'s chain.
    pub fn in_chain(e: &anyhow::Error) -> Option<&Self> {
        e.chain().find_map(|c| c.downcast_ref::<Self>())
    }
}

impl GoogleApi {
    pub async fn get_json(&self, url: &str, lk: &LatchkeySettings) -> Result<Value> {
        let shown = ShownUrl::new(self.service, url);
        let req = HttpRequest::get(self.service, url)
            .header("Accept", "application/json")
            .latchkey(lk.clone());
        let resp = latchkey_curl_classified(&req, retryability)
            .await
            .with_context(|| format!("GET {shown}"))?;
        if resp.status != 200 {
            return Err(self.error(shown, &resp).into());
        }
        serde_json::from_slice(&resp.body).with_context(|| format!("parse JSON from {shown}"))
    }

    fn error(&self, url: ShownUrl, resp: &HttpResponse) -> ApiError {
        let body = resp.body_str();
        let parsed: Option<Value> = serde_json::from_str(&body).ok();
        let said = parsed
            .as_ref()
            .and_then(|v| v.pointer("/error/message")?.as_str().map(str::to_string))
            .unwrap_or_else(|| body.chars().take(300).collect());
        let means = Upstream::of_status(resp.status);
        let message = match means {
            Some(Upstream::CredentialRefused) => format!(
                "{said}. {}.",
                crate::latchkey::sign_in_advice(&format!(
                    "Log in with `latchkey auth browser {}` and approve every scope it asks for",
                    self.latchkey_service
                ))
            ),
            _ => said,
        };
        ApiError {
            status: resp.status,
            reason: parsed.as_ref().and_then(reason),
            message,
            url,
            means,
        }
    }
}

/// The reason an error body gives: in an `ErrorInfo` detail from the
/// newer APIs, in the first of `errors` from the older ones.
fn reason(body: &Value) -> Option<String> {
    let details = body.pointer("/error/details").and_then(Value::as_array);
    details
        .into_iter()
        .flatten()
        .find_map(|d| d.get("reason"))
        .or_else(|| body.pointer("/error/errors/0/reason"))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Google answers a per-user rate limit with a 403 whose body says so;
/// that one is worth waiting out. Every other 403 is about access.
pub fn retryability(resp: &HttpResponse) -> Retryability {
    let body = resp.body_str();
    if resp.status == 403
        && (body.contains("rateLimitExceeded") || body.contains("userRateLimitExceeded"))
    {
        return Retryability::Retry { retry_after: None };
    }
    default_retryability(resp)
}

/// An https link on `googleusercontent.com`, where Google serves bytes
/// to whoever holds the link: it goes out plain, with no credential.
pub fn is_googleusercontent(link: &str) -> bool {
    let Ok(url) = url::Url::parse(link) else {
        return false;
    };
    let host = url.host_str().unwrap_or("");
    url.scheme() == "https"
        && (host == "googleusercontent.com" || host.ends_with(".googleusercontent.com"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn response(status: u16, body: &str) -> HttpResponse {
        HttpResponse {
            status,
            headers: Default::default(),
            body: body.as_bytes().to_vec(),
            duration_ms: 0,
        }
    }

    const API: GoogleApi = GoogleApi {
        service: HttpService::GoogleCalendar,
        latchkey_service: "google-calendar",
    };

    fn error(status: u16, body: &str) -> ApiError {
        let url = ShownUrl::new(API.service, "https://www.googleapis.com/x?syncToken=s");
        API.error(url, &response(status, body))
    }

    #[test]
    fn a_403_is_retried_only_when_it_is_a_rate_limit() {
        let of = |body: &str| retryability(&response(403, body));
        assert!(matches!(
            of(r#"{"error":{"errors":[{"reason":"rateLimitExceeded"}]}}"#),
            Retryability::Retry { .. }
        ));
        assert!(matches!(
            of(r#"{"error":{"errors":[{"reason":"insufficientPermissions"}]}}"#),
            Retryability::Accept
        ));
    }

    /// Both spellings of a reason are read, and the query, which carries
    /// the sync token, is never shown.
    #[test]
    fn an_error_reply_says_what_google_said_and_why() {
        let older = error(
            410,
            r#"{"error":{"code":410,"message":"Sync token is no longer valid.","errors":[{"reason":"fullSyncRequired"}]}}"#,
        );
        assert_eq!(older.reason.as_deref(), Some("fullSyncRequired"));
        let newer = error(
            400,
            r#"{"error":{"code":400,"message":"Sync token is expired.","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"EXPIRED_SYNC_TOKEN"}]}}"#,
        );
        assert_eq!(newer.reason.as_deref(), Some("EXPIRED_SYNC_TOKEN"));
        let e = anyhow::Error::new(newer).context("list");
        assert_eq!(ApiError::in_chain(&e).unwrap().status, 400);
        assert_eq!(Upstream::in_chain(&e), None);
        assert!(!format!("{e:#}").contains("syncToken"), "{e:#}");
    }

    #[test]
    fn a_refused_credential_says_where_to_sign_in() {
        let e = anyhow::Error::new(error(403, r#"{"error":{"message":"Insufficient scopes"}}"#));
        assert_eq!(Upstream::in_chain(&e), Some(Upstream::CredentialRefused));
        let said = format!("{e:#}");
        assert!(said.contains("Insufficient scopes. "), "{said}");
    }

    /// A redirect's `Location` and a contact's photo link come from
    /// upstream and go to plain curl.
    #[test]
    fn only_an_https_googleusercontent_link_goes_out_plain() {
        for yes in [
            "https://lh3.googleusercontent.com/contacts/picard=s100",
            "https://doc-0c-docstext.googleusercontent.com/export/a?b=c",
            "https://googleusercontent.com/a",
        ] {
            assert!(is_googleusercontent(yes), "{yes}");
        }
        for no in [
            "http://doc-0c-docstext.googleusercontent.com/export/a",
            "https://www.googleapis.com/a",
            "https://googleusercontent.com.evil.example/a",
            "https://evilgoogleusercontent.com/a",
            "https://doc.googleusercontent.com@evil.example/a",
            "file:///etc/passwd",
            "-K/etc/passwd",
            "/export/a",
        ] {
            assert!(!is_googleusercontent(no), "{no}");
        }
    }
}
