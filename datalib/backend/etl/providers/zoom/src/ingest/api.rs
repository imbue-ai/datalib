//! Zoom's REST API as this source reads it: every URL it asks for, and
//! how an answer is judged. Every request is built here, so a test's
//! playback fixture is keyed the way the download asks.
//!
//! latchkey signs `https://api.zoom.us/v2/` only. A recording's
//! `download_url` is on `zoom.us`, so it goes out plain, carrying the
//! download token Zoom hands out beside the meeting's files.

use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use serde_json::Value;

use datalib_etl_web::http::{
    default_retryability, latchkey_curl_classified, percent_encode, HttpError, HttpRequest,
    HttpResponse, HttpService, LatchkeySettings,
};

use super::shape::uuid_segment;

pub const HTTP_SERVICE: HttpService = HttpService::Zoom;
pub const BASE: &str = "https://api.zoom.us/v2";

/// Zoom's largest page for every list this source reads.
pub const PAGE: u32 = 300;

/// A video file runs to hundreds of MB.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(1800);
const JSON_TIMEOUT: Duration = Duration::from_secs(120);

// ── URLs ─────────────────────────────────────────────────────────────

pub fn user_url(user: &str) -> String {
    format!("{BASE}/users/{}", percent_encode(user))
}

/// The account's users, for an account with no `api.user` whose sign-in
/// has no user of its own.
pub fn users_url() -> String {
    format!("{BASE}/users?status=active&page_size=2")
}

/// One page of a dated listing: `listing` is one of [`Listing`].
pub fn listing_url(
    listing: Listing,
    user_id: &str,
    from: &str,
    to: &str,
    page_token: Option<&str>,
) -> String {
    let user = percent_encode(user_id);
    let mut url = match listing {
        Listing::Recordings => {
            format!("{BASE}/users/{user}/recordings?from={from}&to={to}&page_size={PAGE}")
        }
        Listing::Reports => format!(
            "{BASE}/report/users/{user}/meetings?from={from}&to={to}&type=past&page_size={PAGE}"
        ),
        Listing::Summaries => format!(
            "{BASE}/users/{user}/meeting_summaries?from={}&to={}&page_size={PAGE}",
            percent_encode(&format!("{from}T00:00:00Z")),
            percent_encode(&format!("{to}T23:59:59Z")),
        ),
    };
    if let Some(token) = page_token {
        url.push_str("&next_page_token=");
        url.push_str(&percent_encode(token));
    }
    url
}

/// A meeting's recordings with the token that downloads them.
pub fn meeting_recordings_url(uuid: &str) -> String {
    format!(
        "{BASE}/meetings/{}/recordings?include_fields=download_access_token",
        uuid_segment(uuid)
    )
}

pub fn participants_url(uuid: &str, page_token: Option<&str>) -> String {
    let mut url = format!(
        "{BASE}/past_meetings/{}/participants?page_size={PAGE}",
        uuid_segment(uuid)
    );
    if let Some(token) = page_token {
        url.push_str("&next_page_token=");
        url.push_str(&percent_encode(token));
    }
    url
}

pub fn summary_url(uuid: &str) -> String {
    format!("{BASE}/meetings/{}/meeting_summary", uuid_segment(uuid))
}

/// The three dated listings, each with its own coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listing {
    /// `GET /users/{user}/recordings`: the meetings with a cloud recording.
    Recordings,
    /// `GET /report/users/{user}/meetings`: past meetings, recorded or not.
    Reports,
    /// `GET /users/{user}/meeting_summaries`: AI Companion summaries.
    Summaries,
}

impl Listing {
    pub fn name(self) -> &'static str {
        match self {
            Listing::Recordings => "recordings",
            Listing::Reports => "reports",
            Listing::Summaries => "summaries",
        }
    }

    /// The list a page carries its entries under.
    pub fn items_key(self) -> &'static str {
        match self {
            Listing::Recordings | Listing::Reports => "meetings",
            Listing::Summaries => "summaries",
        }
    }
}

// ── Requests ─────────────────────────────────────────────────────────

/// Why a request came to nothing.
#[derive(thiserror::Error, Debug)]
pub enum ApiError {
    /// Nothing else will fare better: latchkey would not send it (no
    /// sign-in), the token was refused, or the app lacks a scope.
    #[error("{0}")]
    Service(String),
    /// A feature of a paid plan, asked of an account without one (Zoom's
    /// code 200): the same for every meeting, so the phase asking ends.
    #[error("{0}")]
    NeedsPaidPlan(String),
    /// 404: no such user, meeting or file, or not one this account sees.
    #[error("{0}")]
    NotFound(String),
    /// Zoom will not give this one: a feature the plan does not include,
    /// a meeting too old for its participants, a summary in the trash.
    #[error("{0}")]
    Unavailable(String),
    /// About this one request: a 5xx past its retries, a network failure.
    #[error("{0}")]
    Request(String),
}

pub fn is_service(e: &anyhow::Error) -> bool {
    matches!(e.downcast_ref::<ApiError>(), Some(ApiError::Service(_)))
}

pub fn needs_paid_plan(e: &anyhow::Error) -> bool {
    matches!(
        e.downcast_ref::<ApiError>(),
        Some(ApiError::NeedsPaidPlan(_))
    )
}

pub fn is_not_found(e: &anyhow::Error) -> bool {
    matches!(e.downcast_ref::<ApiError>(), Some(ApiError::NotFound(_)))
}

pub fn is_unavailable(e: &anyhow::Error) -> bool {
    matches!(e.downcast_ref::<ApiError>(), Some(ApiError::Unavailable(_)))
}

pub fn req_json(url: &str) -> HttpRequest {
    HttpRequest::get(HTTP_SERVICE, url)
        .header("Accept", "application/json")
        .timeout(JSON_TIMEOUT)
}

/// A recording file's download. `token` is the meeting's
/// `download_access_token`; it goes in a header, the way Zoom documents,
/// since `zoom.us` is not a host latchkey signs.
pub fn req_download(url: &str, token: &str) -> HttpRequest {
    HttpRequest::get(HTTP_SERVICE, url)
        .header("Authorization", format!("Bearer {token}"))
        .timeout(DOWNLOAD_TIMEOUT)
        .plain()
}

/// Where a download redirected to: signed already, so it goes without
/// the token, which stays with the host it was made for.
pub fn req_redirected(url: &str) -> HttpRequest {
    HttpRequest::get(HTTP_SERVICE, url)
        .timeout(DOWNLOAD_TIMEOUT)
        .plain()
}

pub async fn get_json(url: &str, lk: &LatchkeySettings) -> Result<Value> {
    let req = req_json(url).latchkey(lk.clone());
    let resp = send(&req).await?;
    if resp.status != 200 {
        return Err(status_error(url, &resp).into());
    }
    serde_json::from_slice(&resp.body).with_context(|| format!("parse JSON from {url}"))
}

/// A recording file's bytes, following one redirect.
pub async fn download(url: &str, token: &str) -> Result<Vec<u8>> {
    let resp = send(&req_download(url, token)).await?;
    match resp.status {
        200 => Ok(resp.body),
        301 | 302 | 303 | 307 | 308 => {
            let to = resp
                .header("location")
                .ok_or_else(|| anyhow!("GET {url}: {} with no Location", resp.status))?;
            let resp = send(&req_redirected(to)).await?;
            if resp.status == 200 {
                Ok(resp.body)
            } else {
                Err(status_error(to, &resp).into())
            }
        }
        _ => Err(status_error(url, &resp).into()),
    }
}

async fn send(req: &HttpRequest) -> Result<HttpResponse> {
    latchkey_curl_classified(req, default_retryability)
        .await
        .map_err(|e| transport_error(e, &req.url).into())
}

/// latchkey exits 1 when it will not send the request at all: no
/// credential for `zoom`. curl's own failures have codes of their own.
fn transport_error(e: HttpError, url: &str) -> ApiError {
    match e {
        HttpError::Curl { exit: 1, .. } | HttpError::Spawn { .. } => {
            ApiError::Service(format!("GET {url}: {e}{SIGN_IN_HINT}"))
        }
        other => ApiError::Request(other.to_string()),
    }
}

const SIGN_IN_HINT: &str = ". Sign in again with `latchkey auth browser zoom`, as a Zoom \
     account admin; it makes a Server-to-Server OAuth app with every admin scope";

/// Zoom's error codes that say the credential, not the request, is the
/// trouble: 124 an invalid or expired token, 4700 and 4711 a token
/// without the scope the endpoint needs.
const CREDENTIAL_CODES: &[i64] = &[124, 4700, 4711];

/// Zoom's code for a feature only a paid plan has.
const PAID_PLAN_CODE: i64 = 200;

/// Zoom's error codes for one thing the account cannot have: 12702
/// participants of a meeting over a year old, 2305 a summary turned off
/// or in the trash, 3301 a recording that is gone, 3322 a transcript
/// that is gone.
const UNAVAILABLE_CODES: &[i64] = &[12702, 2305, 3301, 3322];

pub fn status_error(url: &str, resp: &HttpResponse) -> ApiError {
    let body = resp.body_str();
    let parsed = serde_json::from_str::<Value>(&body).ok();
    let code = parsed
        .as_ref()
        .and_then(|v| v.get("code"))
        .and_then(Value::as_i64);
    let message = parsed
        .as_ref()
        .and_then(|v| v.get("message"))
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| body.chars().take(300).collect());
    let said = match code {
        Some(code) => format!("GET {url}: {} (Zoom code {code}): {message}", resp.status),
        None => format!("GET {url}: {}: {message}", resp.status),
    };
    let is = |codes: &[i64]| code.is_some_and(|c| codes.contains(&c));
    if resp.status == 401 || is(CREDENTIAL_CODES) {
        ApiError::Service(format!("{said}{SIGN_IN_HINT}"))
    } else if is(&[PAID_PLAN_CODE]) {
        ApiError::NeedsPaidPlan(said)
    } else if is(UNAVAILABLE_CODES) {
        ApiError::Unavailable(said)
    } else if resp.status == 404 {
        ApiError::NotFound(said)
    } else {
        ApiError::Request(said)
    }
}

/// The entries of one page of a Zoom list under `key`, and the token of
/// the next page. A page without `key` is not a listing at all.
pub fn page_items(v: &Value, key: &str) -> Result<(Vec<Value>, Option<String>)> {
    let items = v
        .get(key)
        .and_then(Value::as_array)
        .cloned()
        .ok_or_else(|| anyhow!("the reply carried no `{key}` list"))?;
    let next = v
        .get("next_page_token")
        .and_then(Value::as_str)
        .filter(|t| !t.is_empty())
        .map(str::to_string);
    Ok((items, next))
}

/// Every page of a Zoom list.
pub async fn paged(
    url: impl Fn(Option<&str>) -> String,
    key: &str,
    lk: &LatchkeySettings,
) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    let mut page: Option<String> = None;
    loop {
        let v = get_json(&url(page.as_deref()), lk).await?;
        let (items, next) = page_items(&v, key).with_context(|| format!("list {key}"))?;
        out.extend(items);
        match next {
            Some(token) => page = Some(token),
            None => return Ok(out),
        }
    }
}

/// Whose meetings to copy, as Zoom's answer for that user. `named` is
/// `api.user`. With none, the sign-in's own user (`/users/me`, which a
/// Server-to-Server app has none of), else the account's only active
/// user.
pub async fn resolve_user(named: Option<&str>, lk: &LatchkeySettings) -> Result<Value> {
    if let Some(user) = named {
        return get_json(&user_url(user), lk).await.map_err(|e| {
            if is_not_found(&e) {
                anyhow!(
                    "{e:#}. `api.user` names a user of the Zoom account the sign-in is for, \
                     by email or Zoom user id"
                )
            } else {
                e
            }
        });
    }
    if let Ok(me) = get_json(&user_url("me"), lk).await {
        return Ok(me);
    }
    let users = get_json(&users_url(), lk).await?;
    let (users, more) = page_items(&users, "users")?;
    match users.as_slice() {
        [only] if more.is_none() => Ok(only.clone()),
        [] => Err(anyhow!(
            "the Zoom account the sign-in is for has no active users; set `api.user`"
        )),
        _ => Err(anyhow!(
            "the Zoom sign-in is for an account with several users, and Zoom does not say \
             which one signed in; set `api.user` to the email of the one whose meetings to copy"
        )),
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
    fn a_request_latchkey_will_not_send_is_service_wide() {
        let e = transport_error(
            HttpError::Curl {
                service: HTTP_SERVICE,
                url: "u".into(),
                exit: 1,
                stderr: "Error: No credentials found for zoom.".into(),
            },
            "u",
        );
        assert!(matches!(e, ApiError::Service(_)), "{e}");
        assert!(e.to_string().contains("latchkey auth browser zoom"), "{e}");
    }

    #[test]
    fn statuses_are_judged_by_zoom_s_code() {
        let url = "https://api.zoom.us/v2/x";
        let judge = |status, body| status_error(url, &resp(status, body));
        assert!(matches!(judge(401, "{}"), ApiError::Service(_)));
        assert!(matches!(
            judge(
                400,
                r#"{"code":4711,"message":"Invalid access token, does not contain scopes."}"#
            ),
            ApiError::Service(_)
        ));
        assert!(matches!(
            judge(
                400,
                r#"{"code":200,"message":"Only available for Paid account."}"#
            ),
            ApiError::NeedsPaidPlan(_)
        ));
        assert!(matches!(
            judge(
                404,
                r#"{"code":3301,"message":"There is no recording for this session."}"#
            ),
            ApiError::Unavailable(_)
        ));
        assert!(matches!(
            judge(404, r#"{"code":1001,"message":"User does not exist."}"#),
            ApiError::NotFound(_)
        ));
        assert!(matches!(judge(500, "oops"), ApiError::Request(_)));
        let e = judge(
            400,
            r#"{"code":12702,"message":"Can not access a meeting a year ago."}"#,
        );
        assert!(e.to_string().contains("Zoom code 12702"), "{e}");
    }

    #[test]
    fn a_listing_asks_for_one_window_and_its_next_page() {
        let url = listing_url(
            Listing::Recordings,
            "u@x.test",
            "2026-09-01",
            "2026-09-28",
            None,
        );
        assert_eq!(
            url,
            "https://api.zoom.us/v2/users/u%40x.test/recordings?from=2026-09-01&to=2026-09-28\
             &page_size=300"
        );
        let url = listing_url(
            Listing::Summaries,
            "U1",
            "2026-09-01",
            "2026-09-28",
            Some("t/1"),
        );
        assert!(url.contains("from=2026-09-01T00%3A00%3A00Z"), "{url}");
        assert!(url.ends_with("&next_page_token=t%2F1"), "{url}");
    }
}
