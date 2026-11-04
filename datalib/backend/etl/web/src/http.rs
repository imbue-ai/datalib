//! Shared HTTP transport for every provider's Download step.
//!
//! One entry point, [`latchkey_curl`], in two modes that produce the same
//! `HttpResponse` so callers cannot tell them apart: **live**, which shells
//! out to `latchkey curl`, and **playback** ([`crate::playback`]), which
//! reads a fixture and never touches the network. We never record live
//! traffic into the repo.
//!
//! It is also the single place rate-limit handling lives.

use std::collections::BTreeMap;
use std::process::Stdio;
use std::time::Duration;

use datalib_etl::upstream::Upstream;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;

/// Re-exported so providers can name the settings they forward without
/// taking a direct dependency on the schema crate — they all already
/// depend on this one.
pub use datalib_source_common::LatchkeySettings;

/// HTTP method. We only model the methods any provider currently issues;
/// extend as needed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum HttpMethod {
    Get,
    Post,
    Propfind,
    Report,
}

impl HttpMethod {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            HttpMethod::Get => "GET",
            HttpMethod::Post => "POST",
            HttpMethod::Propfind => "PROPFIND",
            HttpMethod::Report => "REPORT",
        }
    }

    fn needs_x_flag(self) -> bool {
        !matches!(self, HttpMethod::Get)
    }
}

/// Which upstream service a request goes to.
///
/// **Not the same vocabulary as `grid_rows.provider`**, and the two must
/// not be merged. One provider can speak to two services (`email` uses
/// both [`HttpService::Jmap`] and [`HttpService::Gmail`]; Notion has an
/// official API and an unofficial one), one service can be reached by
/// providers that share no code, and plenty of providers — anything
/// file-backed — never make a request at all. This names the *service*:
/// which latchkey credential applies, which playback-fixture directory
/// the response is stored under, and whether the request needs Chrome
/// impersonation to get past a JA3 wall.
///
/// The strings are directory names under a playback root, so changing
/// one orphans every fixture recorded under the old spelling.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Hash,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
    strum::Display,
)]
#[strum(serialize_all = "snake_case")]
pub enum HttpService {
    /// Calendars over CalDAV (Fastmail, iCloud, Nextcloud, …).
    Caldav,
    /// Calendly's REST API (`api.calendly.com`), the `calendar` source's
    /// `calendly` mode.
    Calendly,
    /// Contacts over CardDAV. Named for the protocol because that is
    /// what latchkey registers.
    Carddav,
    Chatgpt,
    Claude,
    /// Garmin Connect (`connectapi.garmin.com`), a latchkey service by
    /// way of its Garmin plugin.
    Garmin,
    Github,
    Gitlab,
    /// The Gmail API, one of the `email` source's three download modes.
    Gmail,
    /// The Google Calendar API, one of the `calendar` source's modes.
    GoogleCalendar,
    /// Drive and the Docs, Sheets and Slides APIs, for `google_drive`.
    /// latchkey signs each host with its own Google service.
    GoogleDrive,
    /// A JMAP server (Fastmail and friends), another `email` mode.
    Jmap,
    /// Linear's GraphQL API (`api.linear.app`).
    Linear,
    /// The official `api.notion.com` REST API. Does **not** impersonate:
    /// the Cloudflare-fronted host was `www.notion.so`, reached only by
    /// the retired unofficial client. `api.notion.com` accepts a vanilla
    /// curl TLS fingerprint, confirmed against a live workspace.
    Notion,
    Slack,
    /// Todoist's API v1 (`api.todoist.com`).
    Todoist,
    /// YoLink's signed-URL CSV downloads (`us.yosmart.com/download/…`):
    /// no credential, the URL itself is signed, so every request goes
    /// out plain.
    Yolink,
    /// Fixture-synthesis tests only.
    #[strum(serialize = "test_provider")]
    Test,
}

impl HttpService {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// Whether this service's requests must carry the
    /// Chrome-impersonation marker, so the router curl hands them to
    /// the impersonating curl: these hosts reject a vanilla curl TLS
    /// fingerprint. Services that return false still go through the
    /// router curl, unmarked, and so use the system curl.
    ///
    /// The single source of truth for which services impersonate.
    pub const fn impersonates(self) -> bool {
        matches!(
            self,
            HttpService::Claude
                | HttpService::Chatgpt
                | HttpService::Slack
                | HttpService::Github
                | HttpService::Gitlab
        )
    }

    /// How many path segments of this service's URLs may be shown, for a
    /// service that signs its URLs in the path rather than the query:
    /// YoLink's `/download/<device>/<signature>` keeps its first two.
    const fn unsigned_path_segments(self) -> Option<usize> {
        match self {
            HttpService::Yolink => Some(2),
            _ => None,
        }
    }
}

/// The part of a request's URL that may be stored or printed: its scheme,
/// host and path. The query, the fragment and any `user:password@` are
/// left off, since a signed URL carries its credential there (S3's
/// `X-Amz-Signature`, Google's export links), and so is the signed end of
/// the path for a service that signs there. Anything that does not parse
/// as `scheme://host…` is shown as nothing at all.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShownUrl(String);

impl ShownUrl {
    pub fn new(service: HttpService, url: &str) -> Self {
        let Some((scheme, rest)) = url.split_once("://") else {
            return Self("(not a URL)".to_string());
        };
        if scheme.is_empty() || !scheme.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Self("(not a URL)".to_string());
        }
        let rest = rest.split(['?', '#']).next().unwrap_or_default();
        let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
        let host = authority.rsplit('@').next().unwrap_or_default();
        let path = match service.unsigned_path_segments() {
            None => path.to_string(),
            Some(keep) => {
                let segments: Vec<&str> = path.split('/').skip(1).collect();
                let mut shown: String = segments
                    .iter()
                    .take(keep)
                    .map(|s| format!("/{s}"))
                    .collect();
                if segments.len() > keep {
                    shown.push_str("/…");
                }
                shown
            }
        };
        Self(format!("{scheme}://{host}{path}"))
    }

    pub fn of(req: &HttpRequest) -> Self {
        Self::new(req.service, &req.url)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ShownUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// The body is read into memory whole, so a plain request past this
/// is refused rather than held.
pub const DEFAULT_MAX_BYTES: u64 = 1 << 30;

/// A single outbound HTTP request. Do **not** include the Authorization
/// header — that is injected by `latchkey` based on the URL host.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub service: HttpService,
    pub method: HttpMethod,
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub body: Option<Vec<u8>>,
    pub timeout: Duration,
    /// Skip the `latchkey` credential-injecting shim and shell out to plain
    /// `curl`, for publicly reachable resources with no auth, which have no
    /// latchkey service registered and would fail the host allowlist. Not part
    /// of [`fixture_key`], so a synthesizer's fixtures match either way.
    pub bypass_latchkey: bool,
    /// The largest body a plain request takes; curl stops past it.
    /// latchkey's own curl is not given it.
    pub max_bytes: u64,
    /// The source's latchkey knobs, forwarded whole from its config's
    /// `latchkey_settings:` block (see
    /// [`datalib_source_common::LatchkeySettings`]) — today just which
    /// stored account to run as, but providers pass the struct rather
    /// than the field so a new knob reaches all of them at once.
    pub latchkey: LatchkeySettings,
}

impl HttpRequest {
    pub fn get(service: HttpService, url: impl Into<String>) -> Self {
        Self {
            service,
            method: HttpMethod::Get,
            url: url.into(),
            headers: BTreeMap::new(),
            body: None,
            timeout: Duration::from_secs(60),
            bypass_latchkey: false,
            max_bytes: DEFAULT_MAX_BYTES,
            latchkey: LatchkeySettings::default(),
        }
    }

    pub fn post_json(service: HttpService, url: impl Into<String>, body: Vec<u8>) -> Self {
        let mut headers = BTreeMap::new();
        headers.insert("Content-Type".into(), "application/json".into());
        Self {
            service,
            method: HttpMethod::Post,
            url: url.into(),
            headers,
            body: Some(body),
            timeout: Duration::from_secs(60),
            bypass_latchkey: false,
            max_bytes: DEFAULT_MAX_BYTES,
            latchkey: LatchkeySettings::default(),
        }
    }

    pub fn header(mut self, name: impl Into<String>, value: impl Into<String>) -> Self {
        self.headers.insert(name.into(), value.into());
        self
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn plain(mut self) -> Self {
        self.bypass_latchkey = true;
        self
    }

    pub fn max_bytes(mut self, max_bytes: u64) -> Self {
        self.max_bytes = max_bytes;
        self
    }

    pub fn latchkey(mut self, settings: LatchkeySettings) -> Self {
        self.latchkey = settings;
        self
    }
}

/// A fully-captured HTTP response. Header names are normalized to
/// lower-case so callers can probe `response.header("link")` regardless
/// of how the upstream server cased them.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    /// Body bytes. Use [`HttpResponse::body_str`] for textual responses.
    pub body: Vec<u8>,
    /// Wall-clock duration of the underlying network exchange. Zero in
    /// playback mode.
    #[serde(default)]
    pub duration_ms: u64,
}

impl HttpResponse {
    pub fn body_str(&self) -> std::borrow::Cow<'_, str> {
        String::from_utf8_lossy(&self.body)
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(|s| s.as_str())
    }
}

/// Display and `source` are written by hand rather than derived: the
/// source is the [`Upstream`] the failure means, which no field holds.
/// A URL is a [`ShownUrl`], so a signed one prints without its signature.
#[derive(Debug)]
pub enum HttpError {
    Spawn {
        service: HttpService,
        message: String,
    },
    Timeout {
        service: HttpService,
        url: ShownUrl,
        timeout_ms: u64,
    },
    /// `latchkey curl` exited non-zero. This is a transport-level failure
    /// (DNS, TLS, allowlist) — *not* a non-2xx HTTP status. For HTTP
    /// errors, inspect `HttpResponse::status` on the Ok path.
    Curl {
        service: HttpService,
        url: ShownUrl,
        exit: i32,
        failure: CurlFailure,
        /// The start of what latchkey or curl printed: their own one-line
        /// messages, which name no credential.
        stderr: String,
    },
    Malformed {
        service: HttpService,
        url: ShownUrl,
        message: String,
    },
    /// latchkey's permissions, or its gateway's, refused the request
    /// before it reached the service.
    NotPermitted {
        service: HttpService,
        url: ShownUrl,
        message: String,
    },
    PlaybackMiss(String),
    PlaybackInvalid(String),
    /// The shared retry loop respected rate limits / backed off but the
    /// orchestrator's give-up policy ([`crate::retry::RetryGuard`]) tripped
    /// before the request ever succeeded. Terminal — the caller should
    /// surface it as a hard error for this source.
    GaveUp {
        service: HttpService,
        url: ShownUrl,
        reason: String,
        /// The status of the last attempt, when it got an answer.
        last_status: Option<u16>,
    },
    /// A plain request's body was larger than its `max_bytes`, and curl
    /// stopped. The same on every try, so it is not retried.
    TooLarge {
        service: HttpService,
        url: ShownUrl,
        max_bytes: u64,
    },
    /// The step was asked to stop while this request waited to retry.
    /// Terminal for the unit of work that made the request, the same as
    /// `GaveUp`; the fetch loop's next check of the stop flag ends the run.
    Interrupted {
        service: HttpService,
        url: ShownUrl,
    },
}

/// What kind of failure a `latchkey curl` exit was, read off what it
/// printed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CurlFailure {
    /// latchkey has no service for the URL.
    NoServiceForUrl,
    /// latchkey holds no credential for the service.
    NoCredential,
    /// latchkey holds none under the account the source names.
    NoCredentialForAccount,
    /// latchkey holds several accounts, and the source names none.
    AccountNotNamed,
    /// latchkey's record says the credential has expired.
    Expired,
    /// latchkey's permissions refused the request.
    NotPermitted,
    /// The latchkey gateway did not answer.
    GatewayUnreachable,
    /// latchkey could not get its encryption key.
    Keychain,
    /// curl's own failure; its exit code says which.
    Curl,
    Unrecognised,
}

impl CurlFailure {
    /// latchkey's own exit code for a request its permissions refuse
    /// (`PERMISSION_DENIED_EXIT_CODE`).
    const LATCHKEY_NOT_PERMITTED_EXIT: i32 = 126;

    pub fn classify(exit: i32, stderr: &str) -> Self {
        let has = |needle: &str| stderr.contains(needle);
        if exit == Self::LATCHKEY_NOT_PERMITTED_EXIT || has("Error: Request not permitted.") {
            CurlFailure::NotPermitted
        } else if has("Error: No service matches URL") {
            CurlFailure::NoServiceForUrl
        } else if has("Error: No credentials found for") {
            CurlFailure::NoCredential
        } else if has("has no credentials stored for account")
            || has("No credentials stored for account")
        {
            CurlFailure::NoCredentialForAccount
        } else if has("Multiple accounts are stored") {
            CurlFailure::AccountNotNamed
        } else if has("Error: Credentials for ") && has(" are expired.") {
            CurlFailure::Expired
        } else if has("Failed to reach latchkey gateway") {
            CurlFailure::GatewayUnreachable
        } else if has("No encryption key available") || has("Failed to decrypt file") {
            CurlFailure::Keychain
        } else if exit != 1 && has("curl: (") {
            CurlFailure::Curl
        } else {
            CurlFailure::Unrecognised
        }
    }
}

impl HttpError {
    /// `latchkey curl` (or plain `curl`) exiting non-zero with `stderr`.
    pub fn curl_exit(service: HttpService, url: &str, exit: i32, stderr: &str) -> Self {
        HttpError::Curl {
            service,
            url: ShownUrl::new(service, url),
            exit,
            failure: CurlFailure::classify(exit, stderr),
            stderr: stderr.chars().take(400).collect(),
        }
    }

    /// What this failure says about the service. A timeout and the curl
    /// exits for a connection that never completed (6 could not resolve
    /// the host, 7 could not connect, 28 timeout, 35 TLS, 52 empty reply,
    /// 55 send, 56 receive) are the service not answering, which is also
    /// what the retry loop retries. 6 is a typo'd host as well as a
    /// network that is not up yet, as after a laptop wakes; the guard's
    /// budget is what ends the first.
    pub fn upstream(&self) -> Option<Upstream> {
        match self {
            HttpError::Timeout { .. } => Some(Upstream::Unavailable),
            HttpError::Curl { exit, .. } if matches!(*exit, 6 | 7 | 28 | 35 | 52 | 55 | 56) => {
                Some(Upstream::Unavailable)
            }
            HttpError::Curl {
                failure: CurlFailure::NotPermitted,
                ..
            } => Some(Upstream::NotPermitted),
            HttpError::Curl {
                failure:
                    CurlFailure::NoServiceForUrl | CurlFailure::NoCredential | CurlFailure::Expired,
                ..
            } => Some(Upstream::CredentialRefused),
            HttpError::NotPermitted { .. } => Some(Upstream::NotPermitted),
            HttpError::GaveUp {
                last_status: Some(429),
                ..
            } => Some(Upstream::RateLimited),
            _ => None,
        }
    }
}

/// A latchkey gateway's refusal: every 403 it sends itself, marked
/// `latchkeyError`, is its permission check (no credentials allowed, the
/// request not permitted, the check failing). The service's own 403 has
/// no such marker and stays the caller's to read.
fn gateway_refusal(req: &HttpRequest, resp: &HttpResponse) -> Option<HttpError> {
    if resp.status != 403 {
        return None;
    }
    let body: serde_json::Value = serde_json::from_slice(&resp.body).ok()?;
    if body
        .get("latchkeyError")
        .and_then(serde_json::Value::as_bool)
        != Some(true)
    {
        return None;
    }
    Some(HttpError::NotPermitted {
        service: req.service,
        url: ShownUrl::of(req),
        message: body
            .get("error")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_string(),
    })
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            HttpError::Spawn { service, message } => {
                write!(f, "{service}: spawn latchkey failed: {message}")
            }
            HttpError::Timeout {
                service,
                url,
                timeout_ms,
            } => write!(
                f,
                "{service}: latchkey curl timed out after {timeout_ms}ms ({url})"
            ),
            HttpError::Curl {
                service,
                url,
                exit,
                stderr,
                ..
            } => write!(f, "{service}: latchkey curl exit {exit} ({url}): {stderr}"),
            HttpError::Malformed {
                service,
                url,
                message,
            } => write!(
                f,
                "{service}: malformed response from latchkey curl ({url}): {message}"
            ),
            HttpError::NotPermitted {
                service,
                url,
                message,
            } => write!(
                f,
                "{service}: latchkey did not permit this request ({url}): {message}"
            ),
            HttpError::PlaybackMiss(m) => write!(f, "playback miss: {m}"),
            HttpError::PlaybackInvalid(m) => write!(f, "playback fixture invalid: {m}"),
            HttpError::GaveUp {
                service,
                url,
                reason,
                ..
            } => write!(f, "{service}: gave up retrying ({url}): {reason}"),
            HttpError::TooLarge {
                service,
                url,
                max_bytes,
            } => write!(
                f,
                "{service}: {url} is larger than {} MiB, the most one request takes",
                max_bytes >> 20
            ),
            HttpError::Interrupted { service, url } => {
                write!(f, "{service}: interrupted while waiting to retry ({url})")
            }
        }
    }
}

impl std::error::Error for HttpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self.upstream()? {
            Upstream::CredentialRefused => Some(&Upstream::CredentialRefused),
            Upstream::NotPermitted => Some(&Upstream::NotPermitted),
            Upstream::RateLimited => Some(&Upstream::RateLimited),
            Upstream::Unavailable => Some(&Upstream::Unavailable),
        }
    }
}

/// A single `Retry-After` wait is capped here so one pathological header
/// (or a far-future `x-ratelimit-reset`) can't park a source for hours. The
/// give-up policy ([`crate::retry::RetryGuard`]) still bounds total effort;
/// this just keeps any *single* sleep sane. Realistic values (seconds to a
/// few minutes) are respected exactly.
const MAX_RETRY_AFTER: Duration = Duration::from_secs(600);

/// What the retry loop should do with one completed attempt's response.
/// Returned by the response classifier passed to [`latchkey_curl_classified`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Retryability {
    /// Definitive — hand the response back to the caller unchanged (any 2xx,
    /// and 4xx the caller wants to inspect like 401/404).
    Accept,
    /// Rate-limited / transient — back off and retry, honoring `retry_after`
    /// (from `Retry-After` or a provider-specific reset header) when known.
    Retry { retry_after: Option<Duration> },
}

/// Default response classifier: HTTP 429 and 502–504 are retryable (honoring
/// `Retry-After`); everything else is accepted. Providers whose rate-limit
/// signal isn't visible by status code — Slack's HTTP-200
/// `error:"ratelimited"`, GitHub's `403 + x-ratelimit-remaining:0` — wrap this
/// and call [`latchkey_curl_classified`].
pub fn default_retryability(resp: &HttpResponse) -> Retryability {
    match resp.status {
        429 | 502 | 503 | 504 => Retryability::Retry {
            retry_after: parse_retry_after(resp.header("retry-after")),
        },
        _ => Retryability::Accept,
    }
}

/// Parse a `Retry-After` header value into a wait duration. Supports the
/// delta-seconds form (`"120"`) and the HTTP-date form
/// (`"Wed, 21 Oct 2015 07:28:00 GMT"`). Capped at [`MAX_RETRY_AFTER`].
/// Returns `None` when absent or unparseable (the loop then backs off).
pub fn parse_retry_after(value: Option<&str>) -> Option<Duration> {
    let v = value?.trim();
    if let Ok(secs) = v.parse::<u64>() {
        return Some(Duration::from_secs(secs).min(MAX_RETRY_AFTER));
    }
    if let Ok(when) = chrono::DateTime::parse_from_rfc2822(v) {
        let delta = when.with_timezone(&chrono::Utc) - chrono::Utc::now();
        let secs = delta.num_seconds().max(0) as u64;
        return Some(Duration::from_secs(secs).min(MAX_RETRY_AFTER));
    }
    None
}

/// The marker header the router curl routes on (`latchkey-curl-router`
/// in github.com/imbue-ai/latchkey-curl-shims). The value `1` asks for
/// the router's default profile; a profile name would select that one.
pub const IMPERSONATE_MARKER_HEADER: &str = "X-Imbue-Impersonate: 1";

/// URL prefix that makes a request leave from the user's own machine rather
/// than from the workspace's own egress. Kept so that workspaces created by
/// a minds that still publishes it keep working; a current minds decides
/// this inside the gateway instead, by the router curl's
/// `LATCHKEY_DESKTOP_PROXY_CONFIG` file, with no help from datalib. Remove
/// this once no supported minds publishes the prefix.
///
/// Only the impersonating providers ask for it: a remote workspace runs on a
/// VPS and reaches third parties through a gateway on that same VPS, so its
/// requests carry a datacenter IP — and the providers that need TLS
/// impersonation are the same ones that block those ranges outright, where a
/// fingerprint fix does not help.
pub const VIA_DESKTOP_URL_PREFIX_ENV: &str = "MINDS_VIA_DESKTOP_URL_PREFIX";

fn maybe_via_desktop_url(url: &str, service: HttpService, bypass_latchkey: bool) -> String {
    if bypass_latchkey || !service.impersonates() {
        return url.to_string();
    }
    match std::env::var(VIA_DESKTOP_URL_PREFIX_ENV) {
        Ok(prefix) if !prefix.is_empty() => format!("{prefix}/{url}"),
        _ => url.to_string(),
    }
}

/// Issue an HTTP request and return the full response, with the shared retry
/// policy applied — 429, 5xx and transient transport failures, honoring
/// `Retry-After` else backing off exponentially, bounded by the ambient
/// [`crate::retry::RetryGuard`].
///
/// The entry point for every provider whose rate-limit signal is expressible
/// by status code; the rest wrap [`default_retryability`] and call
/// [`latchkey_curl_classified`].
pub async fn latchkey_curl(req: &HttpRequest) -> Result<HttpResponse, HttpError> {
    latchkey_curl_classified(req, default_retryability).await
}

/// Like [`latchkey_curl`], but the caller supplies a response classifier so
/// a provider can mark responses the status code alone can't (e.g. Slack's
/// HTTP-200 `ratelimited` body) as retryable. Transport-level transients
/// (curl exit 7/28/35/56, timeouts) are always retried regardless of the
/// classifier, since there is no response to classify.
pub async fn latchkey_curl_classified<F>(
    req: &HttpRequest,
    classify: F,
) -> Result<HttpResponse, HttpError>
where
    F: Fn(&HttpResponse) -> Retryability,
{
    // Resolve the current source's give-up guard (or a default-bounded
    // throwaway outside any download scope). It owns both the give-up limits
    // and the backoff schedule.
    let guard = crate::retry::current_or_default();
    let mut backoff = guard.initial_backoff();
    loop {
        // Held for the attempt only, never through a backoff, so a request
        // waiting to retry leaves its slot to one that can go now.
        let slot = guard.request_slot().await;
        // A source that gave up asks for nothing more this run: every other
        // request would meet the same dead service, and a loop that went
        // on would fail each of its units in turn and still end
        // `succeeded`. The stop ends every loop at its next boundary,
        // sealed, and the step ends `incomplete` saying why. It waits for
        // this request, not the one that gave up, so that one's caller
        // still reports it as itself.
        if let Some(why) = guard.gave_up() {
            guard.stop().give_up(why);
        }
        // Once the step is asked to stop, no new request leaves. Every
        // page loop in every API provider funnels through here, so this
        // is what ends a half-walked channel or thread without each loop
        // carrying the flag; the unit fails as `Interrupted`, the loop
        // above it sees the flag, and the run ends at the last unit that
        // completed.
        if guard.stop().requested() {
            return Err(HttpError::Interrupted {
                service: req.service,
                url: ShownUrl::of(req),
            });
        }
        // Count every outbound *attempt* against the current source's
        // download metrics (no-op outside an download scope). This is the
        // single transport chokepoint every provider's API call funnels
        // through, so the per-source API-call total is captured here with
        // zero provider-side code. File-based ingestion never reaches this
        // path, so it correctly reports zero requests.
        datalib_etl::download_metrics::record_api_request();
        let in_flight = datalib_etl::download_metrics::request_in_flight();
        let started = std::time::Instant::now();
        let playback = crate::playback::current();
        let left_the_machine = playback.is_none();
        let outcome = match playback {
            None => live::send(req).await,
            Some(playback) => playback.answer(req, guard.stop()).await,
        };
        let outcome = match outcome {
            Ok(resp) => gateway_refusal(req, &resp).map_or(Ok(resp), Err),
            Err(e) => Err(e),
        };
        drop((slot, in_flight));
        log_attempt(req, &outcome, started.elapsed(), left_the_machine);

        // Decide whether this attempt is retryable. `None` = accept (success
        // or a definitive response/error → hand back to the caller as-is);
        // `Some(retry_after)` = retryable failure.
        let retry: Option<Option<Duration>> = match &outcome {
            Ok(resp) => match classify(resp) {
                Retryability::Accept => None,
                Retryability::Retry { retry_after } => Some(retry_after),
            },
            // The service did not answer: retry without a server-specified
            // delay. Everything else is terminal and returned as-is.
            Err(e) if e.upstream() == Some(Upstream::Unavailable) => Some(None),
            Err(_) => None,
        };

        let Some(retry_after) = retry else {
            // Accepted: hand it back untouched. Only an answer is
            // progress; a terminal error is not, or a source whose every
            // request fails at once would never run out of time.
            if outcome.is_ok() {
                guard.on_progress();
            }
            return outcome;
        };

        // Retryable failure: consult the give-up guard before sleeping.
        if let crate::retry::GuardVerdict::GiveUp(reason) = guard.on_failure() {
            let detail = match &outcome {
                Ok(resp) => format!("HTTP {}", resp.status),
                Err(e) => e.to_string(),
            };
            guard.give_up(format!(
                "gave up on {} after {reason}; last attempt: {detail}",
                req.service
            ));
            return Err(HttpError::GaveUp {
                service: req.service,
                url: ShownUrl::of(req),
                reason: format!("{reason}; last attempt: {detail}"),
                last_status: outcome.as_ref().ok().map(|resp| resp.status),
            });
        }

        let wait = retry_after
            .map(|d| d.min(MAX_RETRY_AFTER))
            .unwrap_or(backoff);
        let next_backoff = std::cmp::min(backoff * 2, guard.max_backoff());
        // Only the retryable shapes reach here: a classified response, a
        // timeout, or one of the transient curl exits.
        let what = match &outcome {
            Ok(resp) => format!("HTTP {}", resp.status),
            Err(HttpError::Timeout { timeout_ms, .. }) => format!("timeout after {timeout_ms}ms"),
            Err(HttpError::Curl { exit, .. }) => format!("curl exit {exit}"),
            Err(e) => e.to_string(),
        };
        let wait_from = match retry_after {
            Some(_) => "the server's Retry-After",
            None => "our backoff",
        };
        let budget = guard.budget();
        tracing::warn!(
            service = %req.service,
            url = %ShownUrl::of(req),
            status = %what,
            wait_ms = wait.as_millis() as u64,
            retry_after_ms = retry_after.map(|d| d.as_millis() as u64),
            backoff_ms = backoff.as_millis() as u64,
            next_backoff_ms = next_backoff.as_millis() as u64,
            max_backoff_ms = guard.max_backoff().as_millis() as u64,
            sequential_failures = budget.sequential_failures,
            max_sequential_failures = budget.max_sequential_failures,
            time_without_progress_s = budget.time_without_progress.as_secs(),
            max_time_without_progress_s = budget.max_time_without_progress.as_secs(),
            "{what} from {}: waiting {:.1}s ({wait_from}; backoff {:.1}s, next {:.1}s, max {}s) \
             before retrying; {} of {} sequential failures, {}s of {}s without progress",
            req.service,
            wait.as_secs_f64(),
            backoff.as_secs_f64(),
            next_backoff.as_secs_f64(),
            guard.max_backoff().as_secs(),
            budget.sequential_failures,
            budget.max_sequential_failures,
            budget.time_without_progress.as_secs(),
            budget.max_time_without_progress.as_secs(),
        );
        if guard.stop().sleep_unless_stopped(wait).await {
            return Err(HttpError::Interrupted {
                service: req.service,
                url: ShownUrl::of(req),
            });
        }
        backoff = next_backoff;
    }
}

/// A plain request whose body passed `max_bytes`, as the transport and
/// playback both report it.
pub(crate) fn too_large(req: &HttpRequest, max_bytes: u64) -> HttpError {
    HttpError::TooLarge {
        service: req.service,
        url: ShownUrl::of(req),
        max_bytes,
    }
}

/// One debug line per attempt, so a slow step's log says which calls
/// its time went to. A request played back from a test's tape left nothing
/// behind it, so it is not counted or called outbound. The `info` lines
/// are the counts ([`crate::outbound`]), which cannot be read off these:
/// a root logging at `info` never makes them.
fn log_attempt(
    req: &HttpRequest,
    outcome: &Result<HttpResponse, HttpError>,
    took: Duration,
    left_the_machine: bool,
) {
    let (status, bytes) = match outcome {
        Ok(resp) => (resp.status.to_string(), resp.body.len()),
        Err(HttpError::Timeout { .. }) => ("timeout".to_string(), 0),
        Err(HttpError::Curl { exit, .. }) => (format!("curl exit {exit}"), 0),
        Err(HttpError::Interrupted { .. }) => ("interrupted".to_string(), 0),
        Err(HttpError::TooLarge { .. }) => ("too large".to_string(), 0),
        Err(_) => ("error".to_string(), 0),
    };
    let url = ShownUrl::of(req);
    let url = url.as_str();
    if !left_the_machine {
        tracing::debug!(
            event = "http_request",
            service = %req.service,
            method = req.method.as_str(),
            url,
            status,
            bytes,
            duration_ms = took.as_millis() as u64,
            "played back {} {url} {status} in {}ms",
            req.method.as_str(),
            took.as_millis(),
        );
        return;
    }
    tracing::debug!(
        target: datalib_log_filter::OUTBOUND_TARGET,
        event = "http_request",
        service = %req.service,
        method = req.method.as_str(),
        url,
        status,
        bytes,
        duration_ms = took.as_millis() as u64,
        "outbound: {} {url} {status} in {}ms",
        req.method.as_str(),
        took.as_millis(),
    );
    let failed = !matches!(outcome, Ok(resp) if resp.status < 400);
    crate::outbound::record(
        crate::outbound::host_of(url),
        crate::outbound::Attempt {
            failed,
            bytes: bytes as u64,
            took,
        },
    );
}

// Live mode: shell out to `latchkey curl`.

mod live {
    use super::*;

    pub(super) async fn send(req: &HttpRequest) -> Result<HttpResponse, HttpError> {
        let body_file = tempfile::NamedTempFile::new().map_err(|e| HttpError::Spawn {
            service: req.service,
            message: format!("tempfile: {e}"),
        })?;
        let body_path = body_file.path().to_path_buf();

        // `latchkey curl …` for the credential-injecting shim, or plain
        // `curl …` when the caller opted out (public, auth-free fetch).
        let mut cmd = if req.bypass_latchkey {
            let mut curl = tokio::process::Command::new("curl");
            curl.args(plain_curl_guards(req.max_bytes));
            curl
        } else {
            // `latchkey [--account <acct>] curl` — the account selector is a
            // latchkey *global* option and so must precede the subcommand;
            // `latchkey_curl_command` is the one place that knows that.
            crate::latchkey::latchkey_curl_command(&req.latchkey).map_err(|e| HttpError::Spawn {
                service: req.service,
                message: format!("{e:#}"),
            })?
        };
        cmd.arg("-sS");
        // -D - dumps the response header block to stdout so we can
        // parse every header. -o <path> writes the body to a tempfile
        // so headers and body are cleanly separated (no need to split
        // on a CRLFCRLF boundary).
        cmd.args(["-D", "-", "-o"]).arg(&body_path);
        if req.method.needs_x_flag() {
            cmd.args(["-X", req.method.as_str()]);
        }
        for (k, v) in &req.headers {
            cmd.arg("-H").arg(format!("{}: {}", k, v));
        }
        // Route CF-fronted providers to the impersonating curl via the
        // router curl's marker header. Only on the latchkey path -- a
        // bypass_latchkey request uses plain curl, which has no router
        // curl to act on the marker and would just send it upstream.
        // The Chrome profile advertises compression and the router hands
        // the reply on as the server sent it, so the decoding is this
        // curl's: through a latchkey gateway, it is the curl at this end
        // that decodes, with the header dump describing the wire body.
        if !req.bypass_latchkey && req.service.impersonates() {
            cmd.arg("--compressed");
            cmd.arg("-H").arg(IMPERSONATE_MARKER_HEADER);
        }
        let writes_body_to_stdin = req.body.is_some();
        if writes_body_to_stdin {
            // `@-` reads the request body from stdin so we don't smuggle
            // it through argv (avoids ARG_MAX and lets binary payloads
            // round-trip cleanly).
            cmd.arg("--data-binary").arg("@-");
            cmd.stdin(Stdio::piped());
        }
        if req.bypass_latchkey {
            // A URL someone else wrote cannot be read as an option.
            cmd.arg("--url");
        }
        cmd.arg(maybe_via_desktop_url(
            &req.url,
            req.service,
            req.bypass_latchkey,
        ));
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let timeout_ms = req.timeout.as_millis() as u64;
        let t0 = std::time::Instant::now();
        let proc_result = tokio::time::timeout(req.timeout, async {
            let mut child = cmd.spawn().map_err(|e| HttpError::Spawn {
                service: req.service,
                message: e.to_string(),
            })?;
            if writes_body_to_stdin {
                if let (Some(mut stdin), Some(payload)) = (child.stdin.take(), req.body.as_ref()) {
                    stdin
                        .write_all(payload)
                        .await
                        .map_err(|e| HttpError::Spawn {
                            service: req.service,
                            message: format!("write stdin: {e}"),
                        })?;
                    stdin.shutdown().await.map_err(|e| HttpError::Spawn {
                        service: req.service,
                        message: format!("close stdin: {e}"),
                    })?;
                }
            }
            child
                .wait_with_output()
                .await
                .map_err(|e| HttpError::Spawn {
                    service: req.service,
                    message: e.to_string(),
                })
        })
        .await
        .map_err(|_| HttpError::Timeout {
            service: req.service,
            url: ShownUrl::of(req),
            timeout_ms,
        })?;
        let proc = proc_result?;
        let duration_ms = t0.elapsed().as_millis() as u64;

        if !proc.status.success() {
            let stderr = String::from_utf8_lossy(&proc.stderr);
            let exit = proc.status.code().unwrap_or(-1);
            if req.bypass_latchkey && past_the_cap(exit, &stderr) {
                return Err(too_large(req, req.max_bytes));
            }
            return Err(HttpError::curl_exit(req.service, &req.url, exit, &stderr));
        }

        let header_dump = String::from_utf8_lossy(&proc.stdout).into_owned();
        let (status, headers) =
            parse_header_block(&header_dump).ok_or_else(|| HttpError::Malformed {
                service: req.service,
                url: ShownUrl::of(req),
                // Not the dump itself: its headers can set a cookie.
                message: "no HTTP status line in curl's header dump".to_string(),
            })?;
        let body = std::fs::read(&body_path).map_err(|e| HttpError::Malformed {
            service: req.service,
            url: ShownUrl::of(req),
            message: format!("read body tempfile: {e}"),
        })?;

        Ok(HttpResponse {
            status,
            headers,
            body,
            duration_ms,
        })
    }

    /// What plain curl is told first, for a URL that carries its own
    /// authority and so came from upstream: no `~/.curlrc` (`-q` works
    /// only as the first argument), https and nothing else, and a body
    /// no larger than the request allows.
    pub(super) fn plain_curl_guards(max_bytes: u64) -> Vec<String> {
        [
            "-q",
            "--proto",
            "=https",
            "--proto-redir",
            "=https",
            "--max-filesize",
        ]
        .into_iter()
        .map(String::from)
        .chain([max_bytes.to_string()])
        .collect()
    }

    /// curl's exit for a body past `--max-filesize`: 63 when the size
    /// was announced, 56 when it was not, which is also a dropped
    /// connection, so the message tells the two apart.
    fn past_the_cap(exit: i32, stderr: &str) -> bool {
        exit == 63 || (exit == 56 && stderr.contains("maximum allowed file size"))
    }

    fn parse_header_block(dump: &str) -> Option<(u16, BTreeMap<String, String>)> {
        let normalized = dump.replace("\r\n", "\n");
        let blocks: Vec<&str> = normalized
            .split("\n\n")
            .filter(|b| !b.trim().is_empty())
            .collect();
        let last = blocks.last()?;
        let mut lines = last.lines();
        let status_line = lines.next()?;
        let parts: Vec<&str> = status_line.split_whitespace().collect();
        if parts.len() < 2 {
            return None;
        }
        let status: u16 = parts[1].parse().ok()?;
        let mut headers = BTreeMap::new();
        for line in lines {
            if let Some((k, v)) = line.split_once(':') {
                headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
            }
        }
        Some((status, headers))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn plain_curl_reads_no_curlrc_and_speaks_only_https() {
            let args = plain_curl_guards(1234);
            assert_eq!(args[0], "-q");
            let joined = args.join(" ");
            assert!(joined.contains("--proto =https"), "{joined}");
            assert!(joined.contains("--proto-redir =https"), "{joined}");
            assert!(joined.ends_with("--max-filesize 1234"), "{joined}");
        }

        fn plain(url: &str) -> HttpRequest {
            HttpRequest::get(HttpService::Notion, url).plain()
        }

        /// A signed URL comes from upstream; plain curl took an http
        /// one, and connected to whatever it named.
        #[tokio::test]
        async fn plain_curl_refuses_http_before_connecting() {
            // Port 1 answers nothing: a connection attempt is exit 7.
            match send(&plain("http://127.0.0.1:1/x")).await {
                Err(HttpError::Curl { exit, stderr, .. }) => {
                    assert_eq!(exit, 1, "{stderr}");
                }
                other => panic!("expected curl to refuse the protocol, got {other:?}"),
            }
        }

        /// A body past the cap is the same on every try: curl's exit
        /// 56 for it, when the size was not known up front, read as a
        /// dropped connection and was retried until the guard gave up.
        /// It is its own error now, which a source can record as a skip.
        #[test]
        fn a_body_past_the_cap_is_its_own_error() {
            let exceeded = "curl: (56) Exceeded the maximum allowed file size (100) with 100 bytes";
            assert!(past_the_cap(56, exceeded));
            assert!(past_the_cap(63, ""));
            assert!(!past_the_cap(
                56,
                "curl: (56) Recv failure: Connection reset by peer"
            ));
            let e = too_large(
                &plain("https://files.example/x?X-Amz-Signature=s"),
                100 << 20,
            );
            assert_eq!(e.upstream(), None);
            let said = e.to_string();
            assert!(said.contains("100 MiB"), "{said}");
            assert!(!said.contains("Signature"), "{said}");
        }

        /// A URL that starts with `-` was an option to curl: this one
        /// printed curl's version and exited 0.
        #[tokio::test]
        async fn plain_curl_reads_a_url_as_a_url() {
            match send(&plain("--version")).await {
                Err(HttpError::Curl { .. }) => {}
                other => panic!("expected curl to refuse the url, got {other:?}"),
            }
        }

        #[test]
        fn parses_simple_header_block() {
            let dump = "HTTP/2 200\r\ncontent-type: application/json\r\nx-ratelimit-remaining: 4999\r\n\r\n";
            let (status, headers) = parse_header_block(dump).unwrap();
            assert_eq!(status, 200);
            assert_eq!(headers.get("content-type").unwrap(), "application/json");
            assert_eq!(headers.get("x-ratelimit-remaining").unwrap(), "4999");
        }

        #[test]
        fn keeps_last_block_after_continue() {
            let dump = "HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 200 OK\r\nlink: <next>; rel=\"next\"\r\n\r\n";
            let (status, headers) = parse_header_block(dump).unwrap();
            assert_eq!(status, 200);
            assert!(headers.get("link").unwrap().contains("rel=\"next\""));
        }
    }
}

/// Stable filename a request maps to in the playback root. Used both by
/// `latchkey_curl` (to look up fixtures) and by per-provider synthesizers
/// (to write them). Format: `<METHOD>-<sha256-prefix>.json`. The sha256
/// digest folds in the canonicalized URL, the headers (other than ones
/// latchkey injects), and the body — so two POSTs to the same endpoint
/// with different payloads land in different files.
pub fn fixture_key(req: &HttpRequest) -> String {
    let mut h = Sha256::new();
    h.update(req.method.as_str().as_bytes());
    h.update(b"\n");
    h.update(canonical_url(&req.url).as_bytes());
    h.update(b"\n");
    for (k, v) in &req.headers {
        h.update(k.to_ascii_lowercase().as_bytes());
        h.update(b": ");
        h.update(v.as_bytes());
        h.update(b"\n");
    }
    h.update(b"\n");
    if let Some(body) = &req.body {
        h.update(body);
    }
    let digest = h.finalize();
    // 32 hex chars (128 bits) — plenty to avoid collisions in any
    // realistic fixture set, short enough to read in a directory listing.
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    format!("{}-{hex}.json", req.method.as_str())
}

/// Percent-encode a path segment or query value: every byte but RFC
/// 3986's unreserved ones, as `%XX`.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Canonicalize a URL for fixture-key hashing: sort query parameters by
/// key. Order shouldn't change a server's response, but client code may
/// emit params in different orders across versions; canonicalizing keeps
/// fixtures stable.
fn canonical_url(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else {
        return url.to_string();
    };
    if query.is_empty() {
        return base.to_string();
    }
    let mut params: Vec<(&str, &str)> = query
        .split('&')
        .map(|p| p.split_once('=').unwrap_or((p, "")))
        .collect();
    params.sort_by(|a, b| a.0.cmp(b.0).then(a.1.cmp(b.1)));
    let rebuilt = params
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&");
    format!("{base}?{rebuilt}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playback::{self, Playback};

    /// latchkey exits 1 for every error of its own, so only its refusal
    /// to send counts as a credential problem. A gateway that could not
    /// be reached is not one, though it exits 1 the same way.
    #[test]
    fn only_latchkeys_refusals_to_send_are_a_refused_credential() {
        let curl = |stderr: &str| {
            HttpError::curl_exit(
                HttpService::Slack,
                "https://slack.com/api/auth.test",
                1,
                stderr,
            )
        };
        for refused in [
            "Error: No service matches URL: https://slack.com/api/auth.test",
            "Error: No credentials found for slack.\nRun 'latchkey auth browser slack' first.",
            "Error: Credentials for slack are expired.\nRun 'latchkey auth browser slack'.",
        ] {
            assert_eq!(
                curl(refused).upstream(),
                Some(Upstream::CredentialRefused),
                "{refused}"
            );
        }
        assert_eq!(curl("Error: Upstream request failed.").upstream(), None);
        let unreachable = HttpError::curl_exit(
            HttpService::Slack,
            "https://slack.com/api/auth.test",
            7,
            "curl: (7) Failed to connect",
        );
        assert_eq!(unreachable.upstream(), Some(Upstream::Unavailable));
    }

    /// latchkey's own message is what a failed request says, after the
    /// URL it was asked for without its query.
    #[test]
    fn a_curl_failure_says_what_latchkey_said() {
        let e = HttpError::curl_exit(
            HttpService::Notion,
            "https://s3.test/f.pdf?X-Amz-Signature=s3cret",
            1,
            "Error: No credentials found for notion.",
        );
        assert_eq!(
            e.to_string(),
            "notion: latchkey curl exit 1 (https://s3.test/f.pdf): \
             Error: No credentials found for notion."
        );
        assert_eq!(e.upstream(), Some(Upstream::CredentialRefused));
    }

    /// A URL is stored and printed without what signs it: the query,
    /// user info, a fragment, and the signed end of a YoLink path.
    #[test]
    fn a_shown_url_keeps_scheme_host_and_path_only() {
        let shown = |service, url: &str| ShownUrl::new(service, url).to_string();
        assert_eq!(
            shown(
                HttpService::Notion,
                "https://prod-files-secure.s3.us-west-2.amazonaws.com/a/b/file.pdf\
                 ?X-Amz-Signature=deadbeef&X-Amz-Credential=AKIA"
            ),
            "https://prod-files-secure.s3.us-west-2.amazonaws.com/a/b/file.pdf"
        );
        assert_eq!(
            shown(HttpService::Caldav, "https://picard:tea@dav.test/cal/#frag"),
            "https://dav.test/cal/"
        );
        assert_eq!(
            shown(
                HttpService::Yolink,
                "https://us.yosmart.com/download/ncc1701/c2lnbmF0dXJl?start=1&end=2"
            ),
            "https://us.yosmart.com/download/ncc1701/…"
        );
        assert_eq!(shown(HttpService::Slack, "api_key=s3cret"), "(not a URL)");
        assert_eq!(
            shown(HttpService::Slack, "https://slack.com"),
            "https://slack.com"
        );
    }

    /// A refusal, a give-up and a timeout print the shown URL, never the
    /// query a presigned link carries.
    #[test]
    fn no_error_prints_the_query() {
        let req = HttpRequest::get(
            HttpService::GoogleDrive,
            "https://docs.google.com/export?id=1&X-Amz-Signature=s3cret",
        );
        for e in [
            HttpError::Timeout {
                service: req.service,
                url: ShownUrl::of(&req),
                timeout_ms: 1,
            },
            HttpError::NotPermitted {
                service: req.service,
                url: ShownUrl::of(&req),
                message: "Error: Request not permitted.".into(),
            },
            HttpError::curl_exit(req.service, &req.url, 28, ""),
        ] {
            assert!(!e.to_string().contains("s3cret"), "{e}");
        }
    }

    fn with_via_desktop_prefix<T>(prefix: Option<&str>, body: impl FnOnce() -> T) -> T {
        static GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _lock = GUARD.lock().unwrap_or_else(|poison| poison.into_inner());
        match prefix {
            Some(value) => std::env::set_var(VIA_DESKTOP_URL_PREFIX_ENV, value),
            None => std::env::remove_var(VIA_DESKTOP_URL_PREFIX_ENV),
        }
        let out = body();
        std::env::remove_var(VIA_DESKTOP_URL_PREFIX_ENV);
        out
    }

    const TEST_VIA_DESKTOP_PREFIX: &str = "https://latchkey-self.invalid/via-desktop";

    #[test]
    fn via_desktop_prefix_wraps_impersonating_providers_when_minds_sets_one() {
        with_via_desktop_prefix(Some(TEST_VIA_DESKTOP_PREFIX), || {
            assert_eq!(
                maybe_via_desktop_url("https://slack.com/api/auth.test", HttpService::Slack, false),
                "https://latchkey-self.invalid/via-desktop/https://slack.com/api/auth.test",
            );
        });
    }

    #[test]
    fn via_desktop_prefix_leaves_the_url_alone_when_it_would_change_nothing() {
        // A local workspace gets an empty prefix, and outside minds the variable
        // is unset. Both must leave the URL byte-identical rather than
        // producing a bare "/".
        for prefix in [Some(""), None] {
            with_via_desktop_prefix(prefix, || {
                assert_eq!(
                    maybe_via_desktop_url(
                        "https://slack.com/api/auth.test",
                        HttpService::Slack,
                        false
                    ),
                    "https://slack.com/api/auth.test",
                );
            });
        }
    }

    #[test]
    fn via_desktop_prefix_is_scoped_to_impersonating_latchkey_requests() {
        with_via_desktop_prefix(Some(TEST_VIA_DESKTOP_PREFIX), || {
            assert_eq!(
                maybe_via_desktop_url("https://example.com/x", HttpService::Notion, false),
                "https://example.com/x",
            );
            assert_eq!(
                maybe_via_desktop_url("https://slack.com/api/auth.test", HttpService::Slack, true),
                "https://slack.com/api/auth.test",
            );
        });
    }

    #[test]
    fn via_desktop_prefix_preserves_the_target_url_verbatim() {
        // The gateway slices the prefix back off the raw path, so anything that
        // re-encoded or normalized the target here would change the request the
        // provider actually receives.
        with_via_desktop_prefix(Some(TEST_VIA_DESKTOP_PREFIX), || {
            for url in [
                "https://slack.com/api/conversations.history?channel=C1&limit=100",
                "https://slack.com/files/a%20b?u=x%2Fy",
            ] {
                let wrapped = maybe_via_desktop_url(url, HttpService::Slack, false);
                assert_eq!(
                    wrapped.strip_prefix(&format!("{TEST_VIA_DESKTOP_PREFIX}/")),
                    Some(url),
                );
            }
        });
    }

    #[test]
    fn fixture_key_is_stable_under_query_param_order() {
        let a = HttpRequest::get(
            HttpService::Slack,
            "https://slack.com/api/conversations.history?channel=C1&limit=100",
        );
        let b = HttpRequest::get(
            HttpService::Slack,
            "https://slack.com/api/conversations.history?limit=100&channel=C1",
        );
        assert_eq!(fixture_key(&a), fixture_key(&b));
    }

    #[test]
    fn fixture_key_distinguishes_methods() {
        let g = HttpRequest::get(HttpService::Notion, "https://api.notion.com/v1/search");
        let p = HttpRequest::post_json(
            HttpService::Notion,
            "https://api.notion.com/v1/search",
            b"{}".to_vec(),
        );
        assert_ne!(fixture_key(&g), fixture_key(&p));
    }

    #[test]
    fn fixture_key_distinguishes_post_bodies() {
        let a = HttpRequest::post_json(
            HttpService::Notion,
            "https://api.notion.com/v1/search",
            b"{\"q\":\"a\"}".to_vec(),
        );
        let b = HttpRequest::post_json(
            HttpService::Notion,
            "https://api.notion.com/v1/search",
            b"{\"q\":\"b\"}".to_vec(),
        );
        assert_ne!(fixture_key(&a), fixture_key(&b));
    }

    /// latchkey's answer when its permissions refuse a request: a 403
    /// from the gateway, exit 126 from a local latchkey. Both read as the
    /// service refusing the credential, so the advice was to sign in
    /// again, which cannot help; granting the permission does.
    #[tokio::test]
    async fn a_request_latchkey_does_not_permit_is_its_own_failure() {
        let dir = tempfile::tempdir().unwrap();
        let url = "https://www.googleapis.com/calendar/v3/users/me/calendarList";
        let req = HttpRequest::get(HttpService::GoogleCalendar, url);
        let answer = |status: u16, body: &str| HttpResponse {
            status,
            headers: BTreeMap::from([("content-type".into(), "application/json".into())]),
            body: body.as_bytes().to_vec(),
            duration_ms: 0,
        };
        let tape = |resp: &HttpResponse| {
            let at = dir.path().join(HttpService::GoogleCalendar.as_str());
            std::fs::create_dir_all(&at).unwrap();
            std::fs::write(
                at.join(fixture_key(&req)),
                serde_json::to_vec(resp).unwrap(),
            )
            .unwrap();
        };

        tape(&answer(
            403,
            r#"{"error":"Error: Request not permitted.","latchkeyError":true}"#,
        ));
        let err = playback::scope(dir.path(), latchkey_curl(&req))
            .await
            .unwrap_err();
        assert_eq!(err.upstream(), Some(Upstream::NotPermitted), "{err}");

        // The service's own 403 is the caller's to read.
        tape(&answer(
            403,
            r#"{"error":{"code":403,"message":"Forbidden"}}"#,
        ));
        let got = playback::scope(dir.path(), latchkey_curl(&req))
            .await
            .unwrap();
        assert_eq!(got.status, 403);

        let local = HttpError::curl_exit(
            HttpService::GoogleCalendar,
            url,
            126,
            "Error: Request not permitted.",
        );
        assert_eq!(local.upstream(), Some(Upstream::NotPermitted));
    }

    #[tokio::test]
    async fn playback_miss_returns_named_error() {
        let dir = tempfile::tempdir().unwrap();
        let req = HttpRequest::get(HttpService::Slack, "https://slack.com/api/auth.test");
        let err = playback::scope(dir.path(), latchkey_curl(&req))
            .await
            .unwrap_err();
        match err {
            HttpError::PlaybackMiss(msg) => assert!(msg.contains("auth.test"), "{msg}"),
            other => panic!("expected PlaybackMiss, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn playback_round_trips_a_synthetic_fixture() {
        let dir = tempfile::tempdir().unwrap();
        let req = HttpRequest::get(HttpService::Slack, "https://slack.com/api/auth.test");
        let key = fixture_key(&req);
        let provider_dir = dir.path().join("slack");
        std::fs::create_dir_all(&provider_dir).unwrap();
        let response = HttpResponse {
            status: 200,
            headers: {
                let mut h = BTreeMap::new();
                h.insert("content-type".into(), "application/json".into());
                h
            },
            body: br#"{"ok":true,"team":"Test"}"#.to_vec(),
            duration_ms: 0,
        };
        std::fs::write(
            provider_dir.join(&key),
            serde_json::to_vec(&response).unwrap(),
        )
        .unwrap();
        let got = playback::scope(dir.path(), latchkey_curl(&req))
            .await
            .unwrap();
        assert_eq!(got.status, 200);
        assert_eq!(got.header("content-type"), Some("application/json"));
        assert_eq!(got.body_str(), r#"{"ok":true,"team":"Test"}"#);
    }

    #[test]
    fn parse_retry_after_handles_seconds_and_garbage() {
        assert_eq!(parse_retry_after(Some("7")), Some(Duration::from_secs(7)));
        assert_eq!(
            parse_retry_after(Some("  12 ")),
            Some(Duration::from_secs(12))
        );
        // Capped at MAX_RETRY_AFTER.
        assert_eq!(parse_retry_after(Some("100000")), Some(MAX_RETRY_AFTER));
        assert_eq!(parse_retry_after(None), None);
        assert_eq!(parse_retry_after(Some("soon")), None);
    }

    #[test]
    fn default_retryability_classifies_status() {
        let mk = |status: u16, ra: Option<&str>| {
            let mut headers = BTreeMap::new();
            if let Some(v) = ra {
                headers.insert("retry-after".to_string(), v.to_string());
            }
            HttpResponse {
                status,
                headers,
                body: Vec::new(),
                duration_ms: 0,
            }
        };
        assert_eq!(default_retryability(&mk(200, None)), Retryability::Accept);
        assert_eq!(default_retryability(&mk(404, None)), Retryability::Accept);
        assert_eq!(
            default_retryability(&mk(429, Some("5"))),
            Retryability::Retry {
                retry_after: Some(Duration::from_secs(5))
            }
        );
        assert_eq!(
            default_retryability(&mk(503, None)),
            Retryability::Retry { retry_after: None }
        );
    }

    /// What the lines come to is only true if a root logging at `info` sees
    /// them: it never makes the per-request `debug` line, and the count must
    /// not hang off it. A played-back request left nothing to count.
    #[test]
    fn a_root_at_info_still_hears_who_a_step_reached() {
        use std::sync::{Arc, Mutex};
        use tracing_subscriber::filter::LevelFilter;
        use tracing_subscriber::layer::{Context, SubscriberExt};

        #[derive(Clone, Default)]
        struct Lines(Arc<Mutex<Vec<(String, String)>>>);
        struct Message(String);
        impl tracing::field::Visit for Message {
            fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
                if field.name() == "message" {
                    self.0 = format!("{value:?}");
                }
            }
        }
        impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Lines {
            fn on_event(&self, event: &tracing::Event<'_>, _: Context<'_, S>) {
                let mut message = Message(String::new());
                event.record(&mut message);
                self.0
                    .lock()
                    .unwrap()
                    .push((event.metadata().target().to_string(), message.0));
            }
        }

        let lines = Lines::default();
        let subscriber = tracing_subscriber::registry()
            .with(LevelFilter::INFO)
            .with(lines.clone());
        let req = HttpRequest::get(
            HttpService::Slack,
            "https://outbound-test.example/api?token=secret",
        );
        let answer: Result<HttpResponse, HttpError> = Ok(HttpResponse {
            status: 200,
            headers: BTreeMap::new(),
            body: b"hello".to_vec(),
            duration_ms: 5,
        });
        tracing::subscriber::with_default(subscriber, || {
            log_attempt(&req, &answer, Duration::from_millis(5), false);
            log_attempt(&req, &answer, Duration::from_millis(5), true);
        });

        let lines = lines.0.lock().unwrap();
        assert_eq!(lines.len(), 1, "{lines:#?}");
        assert_eq!(lines[0].0, "http.outbound");
        assert!(
            lines[0]
                .1
                .starts_with("outbound: 1 request to outbound-test.example: 0 failed"),
            "{lines:#?}"
        );
        assert!(!lines[0].1.contains("secret"), "{lines:#?}");
    }

    /// A playback fixture that always returns 429 should be retried until
    /// the ambient guard's sequential-failure limit trips, then surface as
    /// `GaveUp`. Uses near-zero backoff so the test is fast.
    #[tokio::test]
    async fn retries_429_then_gives_up_per_guard() {
        let dir = tempfile::tempdir().unwrap();
        let req = HttpRequest::get(HttpService::Slack, "https://slack.com/api/auth.test");
        let key = fixture_key(&req);
        let provider_dir = dir.path().join("slack");
        std::fs::create_dir_all(&provider_dir).unwrap();
        let response = HttpResponse {
            status: 429,
            headers: BTreeMap::new(),
            body: b"slow down".to_vec(),
            duration_ms: 0,
        };
        std::fs::write(
            provider_dir.join(&key),
            serde_json::to_vec(&response).unwrap(),
        )
        .unwrap();

        let fast = Duration::from_millis(1);
        let guard = crate::retry::RetryGuard::new(
            Duration::from_secs(3600),
            3,
            fast,
            fast,
            datalib_etl::stop::StopFlag::default(),
        );
        let err = playback::scope(
            dir.path(),
            crate::retry::scope(guard, async { latchkey_curl(&req).await }),
        )
        .await
        .unwrap_err();
        match err {
            HttpError::GaveUp { reason, .. } => {
                assert!(reason.contains("sequential failed requests"), "{reason}");
                assert!(reason.contains("HTTP 429"), "{reason}");
            }
            other => panic!("expected GaveUp, got {other:?}"),
        }
    }

    /// A laptop waking from sleep has no DNS for a few seconds, and curl
    /// says exit 6. That failed the request at once, so a source failed
    /// each of its units in turn and still ended `succeeded`. It is
    /// retried now, and a network that comes back is ridden through.
    #[tokio::test]
    async fn a_host_that_does_not_resolve_yet_is_retried() {
        let dir = tempfile::tempdir().unwrap();
        let req = HttpRequest::get(HttpService::Slack, "https://slack.com/api/auth.test");
        ok_fixture(dir.path(), &req);
        let offline = dir.path().join("offline");
        std::fs::write(&offline, "").unwrap();

        let stop = datalib_etl::stop::StopFlag::new();
        let fast = Duration::from_millis(1);
        let guard = crate::retry::RetryGuard::new(Duration::from_secs(3600), 100, fast, fast, stop);
        let back = offline.clone();
        let answer = playback::scope(
            playback::Playback::at(dir.path()).offline(&offline),
            crate::retry::scope(guard.clone(), async {
                let request = latchkey_curl(&req);
                tokio::pin!(request);
                // Back online once the request has failed at least once.
                loop {
                    tokio::select! {
                        answer = &mut request => break answer,
                        _ = tokio::time::sleep(Duration::from_millis(5)) => {
                            if guard.budget().sequential_failures > 0 {
                                let _ = std::fs::remove_file(&back);
                            }
                        }
                    }
                }
            }),
        )
        .await;
        assert_eq!(answer.unwrap().status, 200);
    }

    /// When the guard gives up, the whole source stops, not only the
    /// request that tripped it: every other unit would meet the same dead
    /// network, fail on its first request, and the run would still end
    /// `succeeded`. The stop says why, so the step ends `incomplete`.
    #[tokio::test]
    async fn a_give_up_stops_the_source_and_says_why() {
        let dir = tempfile::tempdir().unwrap();
        let req = HttpRequest::get(HttpService::Slack, "https://slack.com/api/auth.test");
        ok_fixture(dir.path(), &req);
        let offline = dir.path().join("offline");
        std::fs::write(&offline, "").unwrap();

        let stop = datalib_etl::stop::StopFlag::new();
        let fast = Duration::from_millis(1);
        let guard =
            crate::retry::RetryGuard::new(Duration::from_secs(3600), 3, fast, fast, stop.clone());
        let playback = playback::Playback::at(dir.path()).offline(&offline);
        let (first, second) = playback::scope(
            playback,
            crate::retry::scope(guard, async {
                let first = latchkey_curl(&req).await;
                // Back online: the source has stopped all the same.
                std::fs::remove_file(&offline).unwrap();
                (first, latchkey_curl(&req).await)
            }),
        )
        .await;
        assert!(
            matches!(first, Err(HttpError::GaveUp { .. })),
            "the request that gave up fails as itself, for its caller to report; got {first:?}"
        );
        let why = stop.gave_up().expect("the next request stopped the source");
        assert!(why.contains("gave up on slack"), "{why}");
        assert!(why.contains("Could not resolve host"), "{why}");
        assert!(
            matches!(second, Err(HttpError::Interrupted { .. })),
            "no request leaves after a give-up; got {second:?}"
        );
    }

    /// A request that fails for good (here a tape with no answer) is not
    /// progress: it used to reset the no-progress clock, so a source whose
    /// every request failed that way never ran out of time.
    #[tokio::test]
    async fn a_terminal_failure_is_not_progress() {
        let dir = tempfile::tempdir().unwrap();
        let req = HttpRequest::get(HttpService::Slack, "https://slack.com/api/auth.test");
        let fast = Duration::from_millis(1);
        let guard = crate::retry::RetryGuard::new(
            Duration::from_secs(3600),
            100,
            fast,
            fast,
            datalib_etl::stop::StopFlag::default(),
        );
        let before = guard.budget().time_without_progress;
        tokio::time::sleep(Duration::from_millis(20)).await;
        let err = playback::scope(
            dir.path(),
            crate::retry::scope(guard.clone(), async { latchkey_curl(&req).await }),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, HttpError::PlaybackMiss(_)), "got {err:?}");
        assert!(
            guard.budget().time_without_progress > before + Duration::from_millis(15),
            "the clock kept running"
        );
    }

    fn always_429(dir: &std::path::Path, req: &HttpRequest) {
        let provider_dir = dir.join("slack");
        std::fs::create_dir_all(&provider_dir).unwrap();
        let response = HttpResponse {
            status: 429,
            headers: BTreeMap::new(),
            body: b"slow down".to_vec(),
            duration_ms: 0,
        };
        std::fs::write(
            provider_dir.join(fixture_key(req)),
            serde_json::to_vec(&response).unwrap(),
        )
        .unwrap();
    }

    /// A stop requested while a request sits in its backoff ends the wait
    /// at once, as `Interrupted` — a rate-limited provider's minute-long
    /// backoff is otherwise exactly what eats the step's grace.
    #[tokio::test]
    async fn a_stop_cuts_a_backoff_short() {
        let dir = tempfile::tempdir().unwrap();
        let req = HttpRequest::get(HttpService::Slack, "https://slack.com/api/auth.test");
        always_429(dir.path(), &req);

        let stop = datalib_etl::stop::StopFlag::new();
        // A backoff long enough that only the stop can end this test in time.
        let long = Duration::from_secs(600);
        let guard =
            crate::retry::RetryGuard::new(Duration::from_secs(3600), 100, long, long, stop.clone());
        let stopper = stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(50)).await;
            stopper.request();
        });
        let started = std::time::Instant::now();
        let err = playback::scope(
            dir.path(),
            crate::retry::scope(guard, async { latchkey_curl(&req).await }),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, HttpError::Interrupted { .. }), "got {err:?}");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the backoff must end on the stop, not run its course"
        );
    }

    fn ok_fixture(dir: &std::path::Path, req: &HttpRequest) {
        let provider_dir = dir.join("slack");
        std::fs::create_dir_all(&provider_dir).unwrap();
        let response = HttpResponse {
            status: 200,
            headers: BTreeMap::new(),
            body: b"{}".to_vec(),
            duration_ms: 0,
        };
        std::fs::write(
            provider_dir.join(fixture_key(req)),
            serde_json::to_vec(&response).unwrap(),
        )
        .unwrap();
    }

    /// A held tape answers nothing until the hold file is gone, then
    /// answers at once.
    #[tokio::test]
    async fn a_hold_file_parks_a_replayed_request_until_removed() {
        let dir = tempfile::tempdir().unwrap();
        let req = HttpRequest::get(HttpService::Slack, "https://slack.com/api/auth.test");
        ok_fixture(dir.path(), &req);
        let hold = dir.path().join("hold");
        std::fs::write(&hold, b"").unwrap();

        let fast = Duration::from_millis(1);
        let guard = crate::retry::RetryGuard::new(
            Duration::from_secs(3600),
            3,
            fast,
            fast,
            datalib_etl::stop::StopFlag::default(),
        );
        let releaser = hold.clone();
        let released_at = std::sync::Arc::new(std::sync::Mutex::new(None));
        let stamp = released_at.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            *stamp.lock().unwrap() = Some(std::time::Instant::now());
            std::fs::remove_file(&releaser).unwrap();
        });
        let resp = playback::scope(
            Playback::at(dir.path()).hold(&hold),
            crate::retry::scope(guard, async { latchkey_curl(&req).await }),
        )
        .await
        .unwrap();
        assert_eq!(resp.status, 200);
        let released = released_at.lock().unwrap().expect("the release ran first");
        assert!(
            released.elapsed() < Duration::from_secs(5),
            "the answer must follow the release, not a timer"
        );
    }

    /// A source's requests wait for one of its slots, however many its
    /// download starts at once: Google Drive's nested fan-out had 31
    /// `latchkey curl` processes out together, 1.7 GB between them.
    #[tokio::test]
    async fn a_source_has_no_more_requests_out_than_its_slots() {
        let dir = tempfile::tempdir().unwrap();
        let req = HttpRequest::get(HttpService::Slack, "https://slack.com/api/auth.test");
        ok_fixture(dir.path(), &req);
        let hold = dir.path().join("hold");
        std::fs::write(&hold, b"").unwrap();

        let fast = Duration::from_millis(1);
        let guard = crate::retry::RetryGuard::new_with_requests_at_once(
            Duration::from_secs(3600),
            3,
            fast,
            fast,
            2,
            datalib_etl::stop::StopFlag::default(),
        );
        let metrics = datalib_etl::download_metrics::DownloadMetrics::new();
        let calls = futures::future::join_all((0..5).map(|_| latchkey_curl(&req)));
        let watch = async {
            let deadline = std::time::Instant::now() + Duration::from_secs(10);
            while metrics.api_requests() < 2 {
                assert!(
                    std::time::Instant::now() < deadline,
                    "two requests never reached the held tape"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            let out_while_held = metrics.api_requests();
            std::fs::remove_file(&hold).unwrap();
            out_while_held
        };
        let (answers, out_while_held) = playback::scope(
            Playback::at(dir.path()).hold(&hold),
            crate::retry::scope(
                guard,
                datalib_etl::download_metrics::scope(metrics.clone(), async {
                    tokio::join!(calls, watch)
                }),
            ),
        )
        .await;
        assert_eq!(out_while_held, 2, "only the two slots' requests went out");
        assert!(answers.iter().all(|a| a.as_ref().unwrap().status == 200));
        assert_eq!(metrics.api_requests(), 5, "the rest went once a slot freed");
    }

    /// A stop while the tape is held ends the wait as `Interrupted`, so a
    /// held step still stops in its grace.
    #[tokio::test]
    async fn a_stop_ends_a_hold() {
        let dir = tempfile::tempdir().unwrap();
        let req = HttpRequest::get(HttpService::Slack, "https://slack.com/api/auth.test");
        ok_fixture(dir.path(), &req);
        let hold = dir.path().join("hold");
        std::fs::write(&hold, b"").unwrap();

        let stop = datalib_etl::stop::StopFlag::new();
        let fast = Duration::from_millis(1);
        let guard =
            crate::retry::RetryGuard::new(Duration::from_secs(3600), 3, fast, fast, stop.clone());
        let stopper = stop.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            stopper.request();
        });
        let err = playback::scope(
            Playback::at(dir.path()).hold(&hold),
            crate::retry::scope(guard, async { latchkey_curl(&req).await }),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, HttpError::Interrupted { .. }), "got {err:?}");
        assert!(
            hold.exists(),
            "the hold was never released; only the stop ended the wait"
        );
    }

    /// Once the process has sealed, the after-seal hold parks a request
    /// exactly like the plain one. The streaming e2e relies on it to keep
    /// a download in flight after its first checkpoint.
    #[tokio::test]
    async fn a_sealed_hold_parks_a_request_once_a_checkpoint_has_landed() {
        let dir = tempfile::tempdir().unwrap();
        let req = HttpRequest::get(HttpService::Slack, "https://slack.com/api/auth.test");
        ok_fixture(dir.path(), &req);
        let hold = dir.path().join("hold");
        std::fs::write(&hold, b"").unwrap();
        datalib_etl::raw_store::record_seal();

        let fast = Duration::from_millis(1);
        let guard = crate::retry::RetryGuard::new(
            Duration::from_secs(3600),
            3,
            fast,
            fast,
            datalib_etl::stop::StopFlag::default(),
        );
        let releaser = hold.clone();
        let released_at = std::sync::Arc::new(std::sync::Mutex::new(None));
        let stamp = released_at.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(300)).await;
            *stamp.lock().unwrap() = Some(std::time::Instant::now());
            std::fs::remove_file(&releaser).unwrap();
        });
        let resp = playback::scope(
            Playback::at(dir.path()).hold_sealed(&hold),
            crate::retry::scope(guard, async { latchkey_curl(&req).await }),
        )
        .await
        .unwrap();
        assert_eq!(resp.status, 200);
        assert!(
            released_at.lock().unwrap().is_some(),
            "answered while the hold file was still there"
        );
    }

    /// Once stopping, no new request leaves: a page loop that does not
    /// read the flag ends at its next request instead of walking on.
    #[tokio::test]
    async fn no_request_leaves_after_a_stop() {
        let dir = tempfile::tempdir().unwrap();
        let req = HttpRequest::get(HttpService::Slack, "https://slack.com/api/auth.test");
        // No fixture written: a request that left would fail as a playback
        // miss, not as Interrupted.
        let stop = datalib_etl::stop::StopFlag::new();
        stop.request();
        let fast = Duration::from_millis(1);
        let guard = crate::retry::RetryGuard::new(Duration::from_secs(3600), 3, fast, fast, stop);
        let err = playback::scope(
            dir.path(),
            crate::retry::scope(guard, async { latchkey_curl(&req).await }),
        )
        .await
        .unwrap_err();
        assert!(matches!(err, HttpError::Interrupted { .. }), "got {err:?}");
    }
}
