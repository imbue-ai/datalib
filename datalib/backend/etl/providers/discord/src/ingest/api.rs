//! Discord's API (`discord.com/api/v9`) through `latchkey curl`, which
//! adds the account's own credential. Every request is a GET and every
//! one is built here, so a test's playback tape is keyed the way the
//! download sends it.

use std::time::Duration;

use serde_json::Value;

use datalib_etl::upstream::Upstream;
use datalib_etl_web::http::{
    default_retryability, latchkey_curl_classified, HttpError, HttpRequest, HttpResponse,
    HttpService, LatchkeySettings, Retryability,
};

pub const BASE: &str = "https://discord.com/api/v9";
pub const HTTP_SERVICE: HttpService = HttpService::Discord;

const TIMEOUT: Duration = Duration::from_secs(60);
const FILE_TIMEOUT: Duration = Duration::from_secs(300);
/// A file that breaks this many times in a row fails alone, and stays
/// owed for the next run, rather than holding up every file after it.
const FILE_ATTEMPTS: u32 = 5;

/// The most messages Discord answers at once.
pub const MESSAGES_PER_PAGE: u32 = 100;
/// The most threads a search answers at once.
pub const THREADS_PER_PAGE: u32 = 25;

/// Discord's error codes for what this account may not see, and for an
/// endpoint only a bot may call.
const MISSING_ACCESS: i64 = 50001;
const MISSING_PERMISSIONS: i64 = 50013;
const ONLY_BOTS: i64 = 20001;
const BOTS_ONLY: i64 = 20002;

#[derive(thiserror::Error, Debug)]
pub enum DiscordError {
    /// No credential latchkey will send, or one Discord refused: nothing
    /// else this run asks will work.
    #[error("{0}")]
    Unauthorized(String),
    /// The shared retry guard gave up on the service.
    #[error("{0}")]
    GaveUp(String, Option<Upstream>),
    /// The request never got an answer to judge.
    #[error(transparent)]
    Transport(HttpError),
    /// The account may not read this one thing.
    #[error("{0}")]
    NoAccess(String),
    /// Discord has no such thing.
    #[error("{0}")]
    Gone(String),
    /// About this one request.
    #[error("{0}")]
    Permanent(String),
}

impl DiscordError {
    /// Going on would only fail every request left.
    pub fn ends_the_run(&self) -> bool {
        matches!(self, Self::Unauthorized(_) | Self::GaveUp(..))
    }

    /// Asking again would be answered the same.
    pub fn is_settled(&self) -> bool {
        matches!(self, Self::NoAccess(_) | Self::Gone(_))
    }

    pub fn upstream(&self) -> Option<Upstream> {
        match self {
            Self::Unauthorized(_) => Some(Upstream::CredentialRefused),
            Self::GaveUp(_, why) => *why,
            Self::Transport(e) => e.upstream(),
            Self::NoAccess(_) | Self::Gone(_) | Self::Permanent(_) => None,
        }
    }
}

fn get(path: String) -> HttpRequest {
    HttpRequest::get(HTTP_SERVICE, format!("{BASE}{path}"))
        .header("Accept", "application/json")
        .timeout(TIMEOUT)
}

/// Who the credential signs in as.
pub fn me_request() -> HttpRequest {
    get("/users/@me".to_string())
}

/// The servers the account is in, at most 200 of them, which is as many
/// as an account may join.
pub fn guilds_request() -> HttpRequest {
    get("/users/@me/guilds".to_string())
}

/// The direct messages open in the account's list.
pub fn dms_request() -> HttpRequest {
    get("/users/@me/channels".to_string())
}

/// Every channel of a server, the ones the account cannot read included.
pub fn channels_request(guild_id: &str) -> HttpRequest {
    get(format!("/guilds/{guild_id}/channels"))
}

pub fn roles_request(guild_id: &str) -> HttpRequest {
    get(format!("/guilds/{guild_id}/roles"))
}

/// The account's own membership of a server.
pub fn member_request(guild_id: &str) -> HttpRequest {
    get(format!("/users/@me/guilds/{guild_id}/member"))
}

/// A channel's threads, the one with the newest message first, archived
/// or not. `/guilds/{id}/threads/active` is a bot's alone.
pub fn threads_request(channel_id: &str, offset: usize) -> HttpRequest {
    get(format!(
        "/channels/{channel_id}/threads/search?sort_by=last_message_time&sort_order=desc\
         &limit={THREADS_PER_PAGE}&offset={offset}"
    ))
}

/// A page of a conversation, newest first: the newest there is, or what
/// is older than `before`.
pub fn messages_request(channel_id: &str, before: Option<u64>) -> HttpRequest {
    let before = before.map(|b| format!("&before={b}")).unwrap_or_default();
    get(format!(
        "/channels/{channel_id}/messages?limit={MESSAGES_PER_PAGE}{before}"
    ))
}

/// The messages about one message, itself among them. A single message
/// has an endpoint of its own, which only a bot may call.
pub fn around_request(channel_id: &str, message_id: u64) -> HttpRequest {
    get(format!(
        "/channels/{channel_id}/messages?limit={MESSAGES_PER_PAGE}&around={message_id}"
    ))
}

/// Where Discord keeps the files messages carry. A link comes from
/// upstream, so only this host is asked, and with no credential: the link
/// is signed.
const FILE_HOST: &str = "https://cdn.discordapp.com/";

/// `None` for a link that is not to Discord's own file host.
pub fn file_request(url: &str, max_bytes: Option<u64>) -> Option<HttpRequest> {
    if !url.starts_with(FILE_HOST) {
        return None;
    }
    let req = HttpRequest::get(HTTP_SERVICE, url)
        .plain()
        .timeout(FILE_TIMEOUT)
        .attempts(FILE_ATTEMPTS);
    Some(match max_bytes {
        Some(max) => req.max_bytes(max),
        None => req,
    })
}

/// When a signed file link stops working, as Unix seconds: its `ex`, in
/// hex. `None` for a link with no `ex`.
pub fn link_expires(url: &str) -> Option<i64> {
    let (_, query) = url.split_once('?')?;
    let ex = query.split('&').find_map(|p| p.strip_prefix("ex="))?;
    i64::from_str_radix(ex, 16).ok()
}

/// `req` sent, and its answer as JSON.
pub async fn send(req: HttpRequest, latchkey: &LatchkeySettings) -> Result<Value, DiscordError> {
    let req = req.latchkey(latchkey.clone());
    let resp = latchkey_curl_classified(&req, retryability)
        .await
        .map_err(|e| transport_error(e, &req.url))?;
    if !(200..300).contains(&resp.status) {
        return Err(status_error(&req.url, &resp));
    }
    serde_json::from_slice(&resp.body)
        .map_err(|e| DiscordError::Permanent(format!("{}: the answer is not JSON: {e}", req.url)))
}

/// Discord names the wait of a rate limit in the body too, in seconds
/// with a fraction the `Retry-After` header may not carry.
fn retryability(resp: &HttpResponse) -> Retryability {
    match default_retryability(resp) {
        Retryability::Retry { retry_after } => Retryability::Retry {
            retry_after: retry_after.max(body_retry_after(resp)),
        },
        other => other,
    }
}

fn body_retry_after(resp: &HttpResponse) -> Option<Duration> {
    let body: Value = serde_json::from_slice(&resp.body).ok()?;
    let secs = body.get("retry_after")?.as_f64()?;
    (secs.is_finite() && secs > 0.0).then(|| Duration::from_secs_f64(secs.min(600.0)))
}

/// latchkey exits 1 when it will not send the request at all: no
/// credential stored for `discord`. curl's own failures have codes of
/// their own.
fn transport_error(e: HttpError, url: &str) -> DiscordError {
    match e {
        HttpError::Curl { exit: 1, .. } | HttpError::Spawn { .. } => {
            DiscordError::Unauthorized(format!("{url}: {e}. {}", sign_in()))
        }
        HttpError::GaveUp { .. } => DiscordError::GaveUp(e.to_string(), e.upstream()),
        other => DiscordError::Transport(other),
    }
}

fn sign_in() -> String {
    datalib_etl_web::latchkey::sign_in_advice("Sign in with `latchkey auth browser discord`")
}

/// What a non-2xx answer means. Discord's errors are JSON with a
/// `message` and a `code`.
pub fn status_error(url: &str, resp: &HttpResponse) -> DiscordError {
    let body = resp.body_str();
    let parsed: Option<Value> = serde_json::from_str(&body).ok();
    let code = parsed
        .as_ref()
        .and_then(|v| v.get("code"))
        .and_then(Value::as_i64);
    let said = parsed
        .as_ref()
        .and_then(|v| {
            let message = v.get("message").and_then(Value::as_str)?;
            Some(format!("{message} [{}]", code.unwrap_or_default()))
        })
        .unwrap_or_else(|| body.chars().take(300).collect());
    let said = format!("{url}: HTTP {}: {said}", resp.status);
    match (resp.status, code) {
        (403, Some(MISSING_ACCESS | MISSING_PERMISSIONS | ONLY_BOTS | BOTS_ONLY)) => {
            DiscordError::NoAccess(said)
        }
        (401 | 403, _) => DiscordError::Unauthorized(format!("{said}. {}", sign_in())),
        (404, _) => DiscordError::Gone(said),
        _ => DiscordError::Permanent(said),
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
    fn a_page_is_the_newest_or_what_is_older() {
        assert_eq!(
            messages_request("100", None).url,
            "https://discord.com/api/v9/channels/100/messages?limit=100"
        );
        assert_eq!(
            messages_request("100", Some(7)).url,
            "https://discord.com/api/v9/channels/100/messages?limit=100&before=7"
        );
    }

    /// A channel the account may not read costs that channel; a refused
    /// credential costs the run. Bodies as the live API answered them.
    #[test]
    fn a_refusal_is_the_channels_or_the_runs() {
        let no = status_error(
            "u",
            &resp(403, r#"{"message": "Missing Access", "code": 50001}"#),
        );
        assert!(no.is_settled() && !no.ends_the_run(), "{no}");
        let gone = status_error(
            "u",
            &resp(404, r#"{"message": "Unknown Channel", "code": 10003}"#),
        );
        assert!(gone.is_settled(), "{gone}");
        for body in [
            r#"{"message": "401: Unauthorized", "code": 0}"#,
            r#"{"message": "You need to verify your account", "code": 40002}"#,
        ] {
            let status = if body.contains("401") { 401 } else { 403 };
            let e = status_error("u", &resp(status, body));
            assert!(e.ends_the_run(), "{e}");
            assert_eq!(e.upstream(), Some(Upstream::CredentialRefused));
            assert!(
                e.to_string().contains("latchkey auth browser discord"),
                "{e}"
            );
        }
    }

    #[test]
    fn a_wait_named_in_the_body_is_honoured() {
        let limited = resp(
            429,
            r#"{"message": "You are being rate limited.", "retry_after": 2.5, "global": false}"#,
        );
        assert_eq!(
            retryability(&limited),
            Retryability::Retry {
                retry_after: Some(Duration::from_millis(2500))
            }
        );
        assert_eq!(retryability(&resp(200, "[]")), Retryability::Accept);
    }

    /// A file's link comes from upstream, so another host is never asked,
    /// however much it looks like Discord's.
    #[test]
    fn only_discords_file_host_is_asked_and_with_no_credential() {
        let req = file_request(
            "https://cdn.discordapp.com/attachments/1/2/a.png?ex=1",
            Some(9),
        );
        let req = req.unwrap();
        assert!(req.bypass_latchkey);
        assert_eq!(req.max_bytes, 9);
        for no in [
            "https://cdn.discordapp.com.evil.test/a.png",
            "http://cdn.discordapp.com/a.png",
            "https://evil.test/https://cdn.discordapp.com/a.png",
            "",
        ] {
            assert!(file_request(no, None).is_none(), "{no}");
        }
    }

    #[test]
    fn a_signed_link_says_when_it_stops_working() {
        let url = "https://cdn.discordapp.com/attachments/1/2/a.png?ex=65f0a0b0&is=1&hm=2&";
        assert_eq!(link_expires(url), Some(0x65f0_a0b0));
        assert_eq!(link_expires("https://cdn.discordapp.com/a.png"), None);
    }
}
