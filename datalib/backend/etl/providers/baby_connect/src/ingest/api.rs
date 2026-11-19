//! The Baby Connect web app (`app.babyconnect.com`) through `latchkey
//! curl`, which adds the session cookies. Baby Connect publishes no API;
//! these are the two requests its own web page makes, read from its
//! scripts. Every request is built here, so a test's playback tape is
//! keyed the way the download asks.
//!
//! The session is two cookies latchkey holds: `seacloud1`, set when a
//! person signs in, and `srf`, the half of a double-submit check whose
//! other half is the token the home page carries. A list request that
//! sends the token without the cookie is answered 401.

use std::time::Duration;

use serde_json::Value;

use datalib_etl::upstream::Upstream;
use datalib_etl_web::http::{
    default_retryability, latchkey_curl_classified, percent_encode, HttpError, HttpMethod,
    HttpRequest, HttpResponse, HttpService, LatchkeySettings,
};

pub const BASE: &str = "https://app.babyconnect.com";
pub const HTTP_SERVICE: HttpService = HttpService::BabyConnect;

const TIMEOUT: Duration = Duration::from_secs(120);

/// The home page bounds what it embeds; a much larger answer is not it.
const HOME_MAX_BYTES: u64 = 16 * 1024 * 1024;

#[derive(thiserror::Error, Debug)]
pub enum BabyConnectError {
    /// No session latchkey will send, or one Baby Connect refused:
    /// nothing else this run asks will work.
    #[error("{0}")]
    Unauthorized(String),
    /// The shared retry guard gave up on the service.
    #[error("{0}")]
    GaveUp(String, Option<Upstream>),
    /// The request never got an answer to judge.
    #[error(transparent)]
    Transport(HttpError),
    /// About this one request.
    #[error("{0}")]
    Permanent(String),
}

impl BabyConnectError {
    /// Going on would only fail every request left.
    pub fn ends_the_run(&self) -> bool {
        matches!(self, Self::Unauthorized(_) | Self::GaveUp(..))
    }

    pub fn upstream(&self) -> Option<Upstream> {
        match self {
            Self::Unauthorized(_) => Some(Upstream::CredentialRefused),
            Self::GaveUp(_, why) => *why,
            Self::Transport(e) => e.upstream(),
            Self::Permanent(_) => None,
        }
    }
}

/// The signed-in home page, which embeds the account's children and the
/// token every list request carries.
pub fn home_request() -> HttpRequest {
    HttpRequest::get(HTTP_SERVICE, format!("{BASE}/home2"))
        .header("Accept", "text/html")
        .timeout(TIMEOUT)
        .max_bytes(HOME_MAX_BYTES)
}

/// Every entry logged on one day, `pdt` the day as Baby Connect writes it
/// (`YYMMDD` as a number). `kid` is one child's id, or `0` for every
/// child, which is what the web page asks when there is more than one.
/// `tzo` is the asking clock's offset; it decides which day is today, and
/// every day asked here is named, so UTC is as good as any.
pub fn list_request(kid: &str, pdt: u32, token: &str) -> HttpRequest {
    let body = format!(
        "Kid={}&pdt={pdt}&past=1&fmt=long&tzo={}&__srf_token__={}",
        percent_encode(kid),
        percent_encode("0;0"),
        percent_encode(token)
    );
    let mut req = HttpRequest::get(HTTP_SERVICE, format!("{BASE}/CmdListW?cmd=StatusList"))
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .timeout(TIMEOUT);
    req.method = HttpMethod::Post;
    req.body = Some(body.into_bytes());
    req
}

/// `req` sent, and its answer's body. Baby Connect answers a signed-out
/// page by sending the browser to `/login`, which curl does not follow.
pub async fn send(
    req: HttpRequest,
    latchkey: &LatchkeySettings,
) -> Result<HttpResponse, BabyConnectError> {
    let req = req.latchkey(latchkey.clone());
    let resp = latchkey_curl_classified(&req, default_retryability)
        .await
        .map_err(|e| transport_error(e, &req.url))?;
    if !(200..300).contains(&resp.status) {
        return Err(status_error(&req.url, &resp));
    }
    Ok(resp)
}

/// `req` sent, and its answer as JSON. Baby Connect reports a refusal in
/// a 200's body, as `{"Code": 401, "Error": "…"}`.
pub async fn send_json(
    req: HttpRequest,
    latchkey: &LatchkeySettings,
) -> Result<Value, BabyConnectError> {
    let url = req.url.clone();
    let resp = send(req, latchkey).await?;
    let value: Value = serde_json::from_slice(&resp.body)
        .map_err(|e| BabyConnectError::Permanent(format!("{url}: the answer is not JSON: {e}")))?;
    match refusal(&value) {
        Some(e) => Err(e.at(&url)),
        None => Ok(value),
    }
}

/// The refusal a JSON answer carries, if it is one.
pub fn refusal(value: &Value) -> Option<Refusal> {
    let said = value.get("Error").and_then(Value::as_str)?;
    let code = value.get("Code").and_then(Value::as_i64);
    Some(Refusal {
        code,
        said: said.to_string(),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: Option<i64>,
    pub said: String,
}

impl Refusal {
    fn at(self, url: &str) -> BabyConnectError {
        let said = format!("{url}: {}", self.said);
        match self.code {
            Some(401 | 403) => BabyConnectError::Unauthorized(format!("{said}. {}", sign_in())),
            _ => BabyConnectError::Permanent(said),
        }
    }
}

/// latchkey exits 1 when it will not send the request at all: no session
/// stored for `babyconnect`. curl's own failures have codes of their own.
fn transport_error(e: HttpError, url: &str) -> BabyConnectError {
    match e {
        HttpError::Curl { exit: 1, .. } | HttpError::Spawn { .. } => {
            BabyConnectError::Unauthorized(format!("{url}: {e}. {}", sign_in()))
        }
        HttpError::GaveUp { .. } => BabyConnectError::GaveUp(e.to_string(), e.upstream()),
        other => BabyConnectError::Transport(other),
    }
}

pub fn sign_in() -> String {
    datalib_etl_web::latchkey::sign_in_advice(
        "Sign in with `latchkey auth browser babyconnect` and tick Remember Me, so the session \
         lasts a year, after registering the service once as INGEST.md shows",
    )
}

/// What a non-2xx answer means. A redirect is Baby Connect sending a
/// signed-out browser to its login page.
pub fn status_error(url: &str, resp: &HttpResponse) -> BabyConnectError {
    let body: String = resp.body_str().chars().take(300).collect();
    let said = format!("{url}: HTTP {}: {body}", resp.status);
    match resp.status {
        301..=303 | 307 | 308 | 401 | 403 => {
            BabyConnectError::Unauthorized(format!("{said}. {}", sign_in()))
        }
        _ => BabyConnectError::Permanent(said),
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
    fn a_day_is_a_form_post_carrying_the_token() {
        let req = list_request("0", 680512, "tok+/=");
        assert_eq!(req.method, HttpMethod::Post);
        assert_eq!(
            req.url,
            "https://app.babyconnect.com/CmdListW?cmd=StatusList"
        );
        let body = String::from_utf8(req.body.unwrap()).unwrap();
        assert_eq!(
            body,
            "Kid=0&pdt=680512&past=1&fmt=long&tzo=0%3B0&__srf_token__=tok%2B%2F%3D"
        );
    }

    #[test]
    fn a_refusal_in_the_body_ends_the_run_and_says_how_to_sign_in() {
        let v: Value =
            serde_json::from_str(r#"{ "Code":401, "Error":"Authentication required"}"#).unwrap();
        let e = refusal(&v).unwrap().at("u");
        assert!(e.ends_the_run());
        assert_eq!(e.upstream(), Some(Upstream::CredentialRefused));
        assert!(
            e.to_string().contains("latchkey auth browser babyconnect"),
            "{e}"
        );
        assert_eq!(refusal(&serde_json::json!({"list": []})), None);
        let other = refusal(&serde_json::json!({"Code": 500, "Error": "oops"}))
            .unwrap()
            .at("u");
        assert!(!other.ends_the_run());
    }

    #[test]
    fn a_redirect_to_the_login_page_is_a_signed_out_session() {
        assert!(status_error("u", &resp(302, "")).ends_the_run());
        assert!(!status_error("u", &resp(500, "boom")).ends_the_run());
        let missing = transport_error(
            HttpError::curl_exit(HTTP_SERVICE, "u", 1, "No credentials found for babyconnect"),
            "u",
        );
        assert!(missing.ends_the_run());
    }
}
