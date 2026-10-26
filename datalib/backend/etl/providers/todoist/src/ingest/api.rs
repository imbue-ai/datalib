//! Todoist's API v1 (`api.todoist.com/api/v1`) through `latchkey curl`,
//! which adds the credential. Every request is built here, so a test's
//! playback tape is keyed the way the download asks.

use std::time::Duration;

use serde_json::Value;

use datalib_etl::events;
use datalib_etl_web::http::{
    default_retryability, latchkey_curl_classified, percent_encode, HttpError, HttpMethod,
    HttpRequest, HttpResponse, HttpService, LatchkeySettings, Retryability,
};

use super::schema_raw::SYNCED;

pub const BASE: &str = "https://api.todoist.com/api/v1";
pub const HTTP_SERVICE: HttpService = HttpService::Todoist;

/// A full sync of a large account comes as one answer.
const TIMEOUT: Duration = Duration::from_secs(300);

/// The most a paginated endpoint returns at once.
pub const PAGE_LIMIT: u32 = 200;

#[derive(thiserror::Error, Debug)]
pub enum TodoistError {
    /// No credential latchkey will send, or one Todoist refused: nothing
    /// else this run asks will work.
    #[error("{0}")]
    Unauthorized(String),
    /// The shared retry guard gave up on the service.
    #[error("{0}")]
    GaveUp(String),
    /// About this one request.
    #[error("{0}")]
    Permanent(String),
}

impl TodoistError {
    /// Going on would only fail every request left.
    pub fn ends_the_run(&self) -> bool {
        matches!(self, Self::Unauthorized(_) | Self::GaveUp(_))
    }
}

/// The resource types the download asks `/sync` for.
pub fn resource_types() -> String {
    let names: Vec<&str> = SYNCED.iter().filter_map(|k| k.sync_key).collect();
    serde_json::to_string(&names).expect("a list of strings serializes")
}

/// `/sync`, form-encoded as the endpoint asks: `*` for everything, or the
/// token the last answer carried for what changed since.
pub fn sync_request(sync_token: &str) -> HttpRequest {
    let body = format!(
        "sync_token={}&resource_types={}",
        percent_encode(sync_token),
        percent_encode(&resource_types())
    );
    let mut req = HttpRequest::get(HTTP_SERVICE, format!("{BASE}/sync"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .timeout(TIMEOUT);
    req.method = HttpMethod::Post;
    req.body = Some(body.into_bytes());
    req
}

/// One page of the tasks completed in `[since, until)`. Todoist answers
/// for at most three months at a time.
pub fn completed_request(since: &str, until: &str, cursor: Option<&str>) -> HttpRequest {
    let mut url = format!(
        "{BASE}/tasks/completed/by_completion_date?since={}&until={}&limit={PAGE_LIMIT}",
        percent_encode(since),
        percent_encode(until),
    );
    if let Some(cursor) = cursor {
        url.push_str("&cursor=");
        url.push_str(&percent_encode(cursor));
    }
    get_request(url)
}

/// Who the credential signs in as.
pub fn user_request() -> HttpRequest {
    get_request(format!("{BASE}/user"))
}

fn get_request(url: String) -> HttpRequest {
    HttpRequest::get(HTTP_SERVICE, url)
        .header("Accept", "application/json")
        .timeout(TIMEOUT)
}

/// `req` sent, and its answer as JSON. A 2xx that is not JSON is an
/// error of its own: nothing here is stored but JSON.
pub async fn send(req: HttpRequest, latchkey: &LatchkeySettings) -> Result<Value, TodoistError> {
    let req = req.latchkey(latchkey.clone());
    let resp = latchkey_curl_classified(&req, retryability)
        .await
        .map_err(|e| transport_error(e, &req.url))?;
    if !(200..300).contains(&resp.status) {
        return Err(status_error(&req.url, &resp));
    }
    events::item_fetched(&req.url, resp.body.len() as u64, resp.duration_ms);
    serde_json::from_slice(&resp.body)
        .map_err(|e| TodoistError::Permanent(format!("{}: the answer is not JSON: {e}", req.url)))
}

/// Todoist may name the wait in its error body (`error_extra.retry_after`,
/// in seconds) rather than in a `Retry-After` header.
fn retryability(resp: &HttpResponse) -> Retryability {
    match default_retryability(resp) {
        Retryability::Retry { retry_after: None } => Retryability::Retry {
            retry_after: body_retry_after(resp),
        },
        other => other,
    }
}

fn body_retry_after(resp: &HttpResponse) -> Option<Duration> {
    let body: Value = serde_json::from_slice(&resp.body).ok()?;
    let secs = body.pointer("/error_extra/retry_after")?.as_u64()?;
    Some(Duration::from_secs(secs.min(600)))
}

/// latchkey exits 1 when it will not send the request at all: no
/// credential stored for `todoist`. curl's own failures have codes of
/// their own.
fn transport_error(e: HttpError, url: &str) -> TodoistError {
    match e {
        HttpError::Curl { exit: 1, .. } | HttpError::Spawn { .. } => {
            TodoistError::Unauthorized(format!("{url}: {e}{SIGN_IN}"))
        }
        HttpError::GaveUp { .. } => TodoistError::GaveUp(e.to_string()),
        other => TodoistError::Permanent(other.to_string()),
    }
}

const SIGN_IN: &str = ". Sign in with `latchkey auth browser todoist`, or paste the API \
    token from Todoist's Settings → Integrations → Developer with `latchkey auth set todoist \
    -H \"Authorization: Bearer <token>\"`";

/// What a non-2xx answer means. Todoist's errors are JSON with an
/// `error` and an `error_tag`.
pub fn status_error(url: &str, resp: &HttpResponse) -> TodoistError {
    let body = resp.body_str();
    let parsed: Option<Value> = serde_json::from_str(&body).ok();
    let said = parsed
        .as_ref()
        .and_then(|v| {
            let error = v.get("error").and_then(Value::as_str)?;
            let tag = v.get("error_tag").and_then(Value::as_str).unwrap_or("");
            Some(format!("{error} [{tag}]"))
        })
        .unwrap_or_else(|| body.chars().take(300).collect());
    let said = format!("{url}: HTTP {}: {said}", resp.status);
    match resp.status {
        401 | 403 => TodoistError::Unauthorized(format!("{said}{SIGN_IN}")),
        _ => TodoistError::Permanent(said),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn resp(status: u16, body: &str) -> HttpResponse {
        HttpResponse {
            status,
            headers: BTreeMap::new(),
            body: body.as_bytes().to_vec(),
            duration_ms: 0,
        }
    }

    #[test]
    fn a_sync_is_a_form_post_naming_every_resource_type() {
        let req = sync_request("*");
        assert_eq!(req.method, HttpMethod::Post);
        assert_eq!(req.url, "https://api.todoist.com/api/v1/sync");
        let body = String::from_utf8(req.body.unwrap()).unwrap();
        assert_eq!(
            body,
            "sync_token=%2A&resource_types=%5B%22user%22%2C%22projects%22%2C%22sections%22%2C\
             %22items%22%2C%22notes%22%2C%22project_notes%22%2C%22labels%22%2C%22collaborators%22%5D"
        );
        assert_eq!(
            req.headers.get("Content-Type").map(String::as_str),
            Some("application/x-www-form-urlencoded")
        );
    }

    #[test]
    fn a_completed_page_carries_its_window_and_cursor() {
        let req = completed_request("2364-01-01T00:00:00Z", "2364-03-25T00:00:00Z", Some("c/2"));
        assert_eq!(
            req.url,
            "https://api.todoist.com/api/v1/tasks/completed/by_completion_date?\
             since=2364-01-01T00%3A00%3A00Z&until=2364-03-25T00%3A00%3A00Z&limit=200&cursor=c%2F2"
        );
    }

    #[test]
    fn a_refused_credential_ends_the_run_and_says_how_to_sign_in() {
        let e = status_error(
            "u",
            &resp(
                401,
                r#"{"error": "Unauthorized", "error_tag": "AUTH_INVALID_TOKEN"}"#,
            ),
        );
        assert!(e.ends_the_run());
        assert!(e.to_string().contains("AUTH_INVALID_TOKEN"), "{e}");
        assert!(
            e.to_string().contains("latchkey auth browser todoist"),
            "{e}"
        );
        let missing = transport_error(
            HttpError::Curl {
                service: HTTP_SERVICE,
                url: "u".into(),
                exit: 1,
                stderr: "No credentials found for todoist".into(),
            },
            "u",
        );
        assert!(missing.ends_the_run());
        let bad = status_error("u", &resp(400, r#"{"error": "Invalid argument value"}"#));
        assert!(!bad.ends_the_run());
    }

    #[test]
    fn a_wait_named_in_the_body_is_honoured() {
        let limited = resp(
            429,
            r#"{"error": "Too many requests", "error_extra": {"retry_after": 7}}"#,
        );
        assert_eq!(
            retryability(&limited),
            Retryability::Retry {
                retry_after: Some(Duration::from_secs(7))
            }
        );
        assert_eq!(retryability(&resp(200, "{}")), Retryability::Accept);
    }
}
