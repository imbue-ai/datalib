//! Per-process API token — the front door's only authentication.
//!
//! The server binds loopback, but loopback is not a security boundary against
//! the *browser*: any page the user has open can `fetch()`
//! `http://127.0.0.1:8731/…`. With no auth that page could `PUT /api/config` a
//! step whose `command` is an arbitrary shell string and then run it.
//!
//! So one random token is minted per process and required on every request,
//! accepted as `Authorization: Bearer`, `X-Datalib-Token`, `?token=` (the
//! launch URL), or the cookie. A request that presents the token by header or
//! query *and* asks for a document gets it back as a cookie, so the rest of
//! the page authenticates with no per-callsite changes.
//!
//! The cookie is the right carrier here, and a token injected into the HTML
//! is not: a page on `evil.com` can neither read ours (`HttpOnly`) nor have
//! the browser attach it to anything but a top-level navigation
//! (`SameSite=Lax`), and DNS rebinding fails closed, whereas a token in the
//! served HTML is readable by anything that can read the page.
//!
//! Behind a reverse proxy that authenticates every request itself and frames
//! the app in its own page, both measures get in the way: the proxy's page
//! is another site, so the browser withholds the `SameSite=Lax` cookie from
//! the frame, and `frame-ancestors 'none'` blanks it. `$DATALIB_TRUSTED_PROXY`
//! names such a proxy ([`TrustedProxy`]). Behind it, a request whose identity
//! header says the requester is the owner is let in without the token, a
//! request it marks as a visitor is refused, and the page's policy leaves
//! `frame-ancestors` to the proxy (`embed::app_csp`). The token keeps
//! working for everything else — an agent's `curl` on the same host — and a
//! request that carries neither is refused as before.
//!
//! Holding a credential is not enough on its own: the cookie and the
//! proxy's header ride along on requests other pages make the browser
//! send. `SameSite=Lax` does not stop a page on another port of
//! `127.0.0.1`, which is the same site, and a page whose own hostname
//! resolves to this address (DNS rebinding) can forge the proxy's header.
//! So [`fetch_policy`] also judges every request by the headers a page
//! cannot set — `Host`, `Sec-Fetch-Site`, `Sec-Fetch-Mode`, `Origin` —
//! before the credential is looked at. Its rules are on the function.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Redirect, Response};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use uuid::Uuid;

/// Env override for the minted token. Set it to share one token between
/// the backend and a separate process that must speak to it (the Vite
/// dev proxy, the Playwright suite).
pub const TOKEN_ENV: &str = "DATALIB_TOKEN";

/// Token file basename inside [`datalib_core::layout::system_dir`].
pub const TOKEN_FILE: &str = "api-token";

/// Query-string key carrying the token on the launch URL.
pub const TOKEN_QUERY_KEY: &str = "token";

/// Header alternative to `Authorization: Bearer …`, for callers whose
/// `Authorization` is already spoken for.
pub const TOKEN_HEADER: &str = "x-datalib-token";

/// Env naming the reverse proxy whose identity header stands in for the
/// token ([`TrustedProxy::as_str`] spellings). Unset: every request needs
/// the token.
pub const TRUSTED_PROXY_ENV: &str = "DATALIB_TRUSTED_PROXY";

/// A reverse proxy this server may stand behind: it authenticates every
/// request before forwarding it, strips any copy of its identity header a
/// client sent, stamps its own, and frames the app in a page of its own.
/// Only such a proxy can be named here — the header is taken at its word.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, strum::EnumString, strum::IntoStaticStr, strum::VariantArray,
)]
#[strum(serialize_all = "snake_case")]
pub enum TrustedProxy {
    /// Imbue Studio's forwarder. `X-Imbue-Identity` is compact JSON with an
    /// `owner` boolean, plus the account's `user_id` and `email` when the
    /// workspace is shared.
    Imbue,
}

impl TrustedProxy {
    pub fn as_str(self) -> &'static str {
        self.into()
    }

    /// `None` for a spelling this build does not know.
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }

    /// `$DATALIB_TRUSTED_PROXY`: unset or empty names no proxy; a spelling
    /// this build does not know is refused rather than read as "none",
    /// since the operator meant to trust something.
    pub fn from_env() -> anyhow::Result<Option<Self>> {
        let Ok(value) = std::env::var(TRUSTED_PROXY_ENV) else {
            return Ok(None);
        };
        let value = value.trim();
        if value.is_empty() {
            return Ok(None);
        }
        match Self::parse(value) {
            Some(proxy) => Ok(Some(proxy)),
            None => anyhow::bail!(
                "{TRUSTED_PROXY_ENV}={value:?} names no proxy this build knows; it knows {}",
                <Self as strum::VariantArray>::VARIANTS
                    .iter()
                    .map(|p| p.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }

    fn identity_header(self) -> &'static str {
        match self {
            TrustedProxy::Imbue => "x-imbue-identity",
        }
    }

    fn requester(self, headers: &HeaderMap) -> Requester {
        let Some(raw) = headers.get(self.identity_header()) else {
            return Requester::Unvouched;
        };
        match self {
            TrustedProxy::Imbue => parse_imbue_identity(raw.as_bytes()),
        }
    }
}

/// What a trusted proxy's identity header says about the requester.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Requester {
    /// No identity header: the request did not come through the proxy.
    Unvouched,
    /// The proxy vouches for the workspace's owner.
    Owner,
    /// Someone the owner shared the workspace with. The whole mirror is
    /// the owner's, so a visitor gets none of it.
    Visitor,
    /// A header the proxy would not have written.
    Malformed,
}

fn parse_imbue_identity(raw: &[u8]) -> Requester {
    #[derive(serde::Deserialize)]
    struct Identity {
        owner: bool,
    }
    match serde_json::from_slice::<Identity>(raw) {
        Ok(Identity { owner: true }) => Requester::Owner,
        Ok(Identity { owner: false }) => Requester::Visitor,
        Err(_) => Requester::Malformed,
    }
}

/// Env listing host names, beyond `localhost`, the names under it and IP
/// literals, that a request's `Host` may carry in proxy mode:
/// comma-separated, each a name or `*.<domain>` for any name under it. For
/// a trusted proxy that passes the browser's own `Host` through.
pub const ALLOWED_HOSTS_ENV: &str = "DATALIB_ALLOWED_HOSTS";

/// The names a request's `Host` may carry in proxy mode, the one mode in
/// which a rebound page has a credential to send ([`fetch_policy`]).
/// `localhost`, every name under it and every IP literal are always among
/// them: DNS rebinding needs a name the attacker controls, and browsers
/// resolve `*.localhost` to loopback themselves, never through DNS. The
/// port is not checked, since it is the one the client dialed — Docker's
/// `-p`, Vite's dev proxy and an SSH tunnel all put a port other than the
/// bound one there.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AllowedHosts {
    names: Vec<String>,
    /// Each with its leading dot: `*.example.com` is `.example.com`.
    suffixes: Vec<String>,
}

impl AllowedHosts {
    pub fn parse(list: &str) -> anyhow::Result<Self> {
        let mut hosts = Self::default();
        for entry in list.split(',').map(str::trim).filter(|e| !e.is_empty()) {
            let entry = entry.to_ascii_lowercase();
            let (wildcard, name) = match entry.strip_prefix("*.") {
                Some(domain) => (true, domain),
                None => (false, entry.as_str()),
            };
            let is_name = !name.is_empty()
                && name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.'));
            if !is_name {
                anyhow::bail!(
                    "{ALLOWED_HOSTS_ENV}: {entry:?} is not a host name or `*.<domain>`; \
                     list names without a scheme or port"
                );
            }
            if wildcard {
                hosts.suffixes.push(format!(".{name}"));
            } else {
                hosts.names.push(name.to_string());
            }
        }
        Ok(hosts)
    }

    /// `$DATALIB_ALLOWED_HOSTS`; unset admits only the names every server
    /// answers to.
    pub fn from_env() -> anyhow::Result<Self> {
        Self::parse(&std::env::var(ALLOWED_HOSTS_ENV).unwrap_or_default())
    }

    fn admits(&self, host: &str) -> bool {
        let Some(name) = host_name(host) else {
            return false;
        };
        if name.starts_with('[') {
            return name[1..name.len() - 1]
                .parse::<std::net::Ipv6Addr>()
                .is_ok();
        }
        let name = name.to_ascii_lowercase();
        name == "localhost"
            || name
                .strip_suffix(".localhost")
                .is_some_and(|sub| !sub.is_empty())
            || name.parse::<std::net::Ipv4Addr>().is_ok()
            || self.names.contains(&name)
            || self
                .suffixes
                .iter()
                .any(|s| name.ends_with(s.as_str()) && name.len() > s.len())
    }
}

/// `Host` without its port, brackets kept on an IPv6 literal; `None` for
/// a value no browser sends.
fn host_name(host: &str) -> Option<&str> {
    let (name, port) = if host.starts_with('[') {
        let end = host.find(']')?;
        (&host[..=end], &host[end + 1..])
    } else {
        match host.rsplit_once(':') {
            Some((name, _)) => (name, &host[name.len()..]),
            None => (host, ""),
        }
    };
    let port_ok = port.is_empty()
        || port
            .strip_prefix(':')
            .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()));
    (!name.is_empty() && port_ok).then_some(name)
}

/// What a request carries that lets it in, as far as [`fetch_policy`]
/// cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Carrier {
    /// The token itself, by header or query: no page can attach it for us.
    Token,
    /// The session cookie, which the browser attaches on its own.
    Cookie,
    /// A trusted proxy's word that the owner sent it — attached by the
    /// proxy to whatever the owner's browser sends, like a cookie.
    ProxyOwner,
    /// Nothing that lets it in; the gate refuses it after the policy.
    Nothing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FetchVerdict {
    Allow,
    ForeignHost,
    FromAnotherSite,
    CookieWriteFromElsewhere,
}

impl FetchVerdict {
    fn refusal(self) -> Option<&'static str> {
        match self {
            FetchVerdict::Allow => None,
            FetchVerdict::ForeignHost => Some(
                "the request's Host does not name this server (a page that pointed its \
                 own name at this address)",
            ),
            FetchVerdict::FromAnotherSite => {
                Some("a page on another site or port made this request")
            }
            FetchVerdict::CookieWriteFromElsewhere => {
                Some("a write with only the session cookie must carry an Origin naming this server")
            }
        }
    }
}

/// Where a request came from, judged by headers a page cannot set:
///
/// 1. In proxy mode, `Host` must name this server ([`AllowedHosts`]), on
///    every route; otherwise a page that points its own name at this
///    address (DNS rebinding) could send the identity header the proxy is
///    trusted for. Outside proxy mode a rebound page has no credential to
///    send: the browser keeps the cookie for the real name and the page
///    does not know the token, so it reaches the public routes alone. A
///    request with no `Host` did not come from a browser, which always
///    sends one, so there is nothing to rebind.
/// 2. A public route needs nothing more.
/// 3. A navigation — `GET` or `HEAD` with `Sec-Fetch-Mode: navigate` — is
///    allowed from anywhere, so a link or the proxy's frame opens the app.
///    That rests on no `GET` route having a side effect, and on every
///    document served as data carrying its `sandbox` policy.
/// 4. Anything else marked `Sec-Fetch-Site: cross-site` or `same-site` (a
///    page on another port of `127.0.0.1` is the same site) is refused;
///    `same-origin` and `none` (typed in the address bar) are not.
/// 5. With no `Sec-Fetch-Site` (`curl`, an older browser), a write that
///    has only the cookie must carry an `Origin` naming the `Host` it was
///    sent to. The token proves more than a cookie can; the proxy has
///    authenticated a non-browser client itself.
pub(crate) fn fetch_policy(
    method: &Method,
    host: Option<&str>,
    headers: &HeaderMap,
    carrier: Carrier,
    public: bool,
    proxy_hosts: Option<&AllowedHosts>,
) -> FetchVerdict {
    let rebound = match (proxy_hosts, host) {
        (Some(hosts), Some(host)) => !hosts.admits(host),
        _ => false,
    };
    if rebound {
        return FetchVerdict::ForeignHost;
    }
    if public {
        return FetchVerdict::Allow;
    }
    let header = |name: &str| headers.get(name).and_then(|v| v.to_str().ok());
    let navigation = (*method == Method::GET || *method == Method::HEAD)
        && header("sec-fetch-mode") == Some("navigate");
    if navigation {
        return FetchVerdict::Allow;
    }
    match header("sec-fetch-site") {
        Some("same-origin") | Some("none") => FetchVerdict::Allow,
        Some(_) => FetchVerdict::FromAnotherSite,
        None if carrier == Carrier::Cookie && !method.is_safe() => {
            let own_origin = host.is_some_and(|host| {
                header("origin").is_some_and(|origin| {
                    origin
                        .strip_prefix("http://")
                        .or_else(|| origin.strip_prefix("https://"))
                        == Some(host)
                })
            });
            if own_origin {
                FetchVerdict::Allow
            } else {
                FetchVerdict::CookieWriteFromElsewhere
            }
        }
        None => FetchVerdict::Allow,
    }
}

/// The `Host` header, or an HTTP/2 request's `:authority`, which hyper
/// leaves in the URI instead.
fn request_host(req: &Request<Body>) -> Option<String> {
    match req.headers().get(header::HOST) {
        Some(v) => Some(String::from_utf8_lossy(v.as_bytes()).into_owned()),
        None => req.uri().authority().map(|a| a.as_str().to_string()),
    }
}

/// What [`require_token`] is configured with: the token, the proxy whose
/// word is taken in its place, if any, and the host names it answers to
/// in proxy mode.
#[derive(Clone)]
pub struct Gate {
    pub token: ApiToken,
    pub trusted_proxy: Option<TrustedProxy>,
    pub hosts: AllowedHosts,
}

fn is_url_safe(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

/// The server's token plus the two paths derived from it: the cookie it
/// mints and the file it's published in. Cheap to clone (one `Arc`).
#[derive(Clone)]
pub struct ApiToken(Arc<Inner>);

struct Inner {
    value: String,
    cookie_name: String,
    token_file: PathBuf,
}

impl ApiToken {
    /// Take `$DATALIB_TOKEN` if set, else mint 244 random bits.
    ///
    /// `root` only fixes where [`Self::write_token_file`] will publish
    /// it; nothing is written here (the data root may not exist yet).
    pub fn mint(root: &Path) -> anyhow::Result<Self> {
        let value = match std::env::var(TOKEN_ENV) {
            Ok(v) if is_url_safe(v.trim()) => v.trim().to_string(),
            Ok(v) if v.trim().is_empty() => anyhow::bail!(
                "{TOKEN_ENV} is set but empty — unset it to get a random \
                 per-process token, or set it to a non-empty value"
            ),
            Ok(v) => anyhow::bail!(
                "{TOKEN_ENV} must be URL-safe (A-Z a-z 0-9 - . _ ~); got {} \
                 disallowed character(s)",
                v.trim()
                    .chars()
                    .filter(|c| !is_url_safe(&c.to_string()))
                    .count()
            ),
            // Two v4 UUIDs = 244 bits of OS randomness, rendered as 64
            // hex chars. `uuid` is already a dependency here, so this
            // buys the entropy without a new crate in the graph.
            Err(_) => format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple()),
        };
        Ok(Self::from_value(value, root))
    }

    pub fn from_value(value: impl Into<String>, root: &Path) -> Self {
        let value = value.into();
        // Cookies ignore the port, so two datalib instances on
        // 127.0.0.1 (the Tauri shell binds :0, dev.sh picks an
        // ephemeral port) would otherwise fight over one cookie name
        // and log each other out. Deriving the name from a hash of the
        // token gives every process its own slot. Truncated to 8 hex
        // chars: enough to not collide, and a preimage of the full
        // token it is not.
        let digest = Sha256::digest(value.as_bytes());
        let cookie_name = format!(
            "datalib_token_{:02x}{:02x}{:02x}{:02x}",
            digest[0], digest[1], digest[2], digest[3]
        );
        Self(Arc::new(Inner {
            value,
            cookie_name,
            token_file: token_file_path(root),
        }))
    }

    pub fn value(&self) -> &str {
        &self.0.value
    }

    pub fn token_file(&self) -> &Path {
        &self.0.token_file
    }

    pub fn write_token_file(&self) -> anyhow::Result<()> {
        let path = self.token_file();
        let dir = path.parent().expect("token file always has a parent");
        std::fs::create_dir_all(dir)
            .map_err(|e| anyhow::anyhow!("create {}: {e}", dir.display()))?;
        // Truncate-then-tighten would leave a window where the old
        // file's mode applies to new bytes, so remove and recreate.
        let _ = std::fs::remove_file(path);
        std::fs::write(path, &self.0.value)
            .map_err(|e| anyhow::anyhow!("write {}: {e}", path.display()))?;
        restrict_to_owner(path)?;
        Ok(())
    }

    fn cookie_header(&self) -> HeaderValue {
        // No `Secure`: we're on plain http over loopback, and `Secure`
        // would make the browser drop the cookie entirely. No
        // `Max-Age`/`Expires` either — a session cookie is right for a
        // token that dies with the process anyway.
        HeaderValue::from_str(&format!(
            "{}={}; Path=/; HttpOnly; SameSite=Lax",
            self.0.cookie_name, self.0.value
        ))
        .expect("token and cookie name are URL-safe ASCII")
    }

    fn credential(&self, req: &Request<Body>) -> Option<Credential> {
        let headers = req.headers();

        if let Some(v) = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|v| {
                v.strip_prefix("Bearer ")
                    .or_else(|| v.strip_prefix("bearer "))
            })
        {
            if self.matches(v.trim()) {
                return Some(Credential::Presented);
            }
        }

        if let Some(v) = headers.get(TOKEN_HEADER).and_then(|v| v.to_str().ok()) {
            if self.matches(v.trim()) {
                return Some(Credential::Presented);
            }
        }

        if let Some(v) = req.uri().query().and_then(query_token) {
            if self.matches(v) {
                return Some(Credential::Query);
            }
        }

        // Several cookies can share the header; scan for ours by name.
        for cookie in headers
            .get_all(header::COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .flat_map(|v| v.split(';'))
        {
            if let Some((name, val)) = cookie.split_once('=') {
                if name.trim() == self.0.cookie_name && self.matches(val.trim()) {
                    return Some(Credential::Cookie);
                }
            }
        }

        None
    }

    /// Constant-time comparison. Over loopback a timing oracle on a
    /// 244-bit secret is not a practical attack, but the comparison is
    /// two lines either way.
    fn matches(&self, candidate: &str) -> bool {
        let expected = self.0.value.as_bytes();
        let got = candidate.as_bytes();
        if expected.len() != got.len() {
            return false;
        }
        let mut diff = 0u8;
        for (a, b) in expected.iter().zip(got) {
            diff |= a ^ b;
        }
        diff == 0
    }
}

/// How a request proved it holds the token. The distinction drives
/// whether we hand back a cookie and whether we redirect.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Credential {
    /// Already has the cookie — nothing to do.
    Cookie,
    /// `Authorization` / `X-Datalib-Token`.
    Presented,
    /// `?token=…` in the URL.
    Query,
}

pub fn token_file_path(root: &Path) -> PathBuf {
    datalib_core::layout::system_dir(root).join(TOKEN_FILE)
}

pub fn restrict_to_owner(path: &Path) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
            .map_err(|e| anyhow::anyhow!("chmod 0600 {}: {e}", path.display()))?;
    }
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

/// The raw value of the `token` key in a query string, if present.
/// Compared verbatim — see [`is_url_safe`] for why no decoding step.
fn query_token(query: &str) -> Option<&str> {
    query
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .find(|(k, _)| *k == TOKEN_QUERY_KEY)
        .map(|(_, v)| v)
}

pub(crate) fn query_without_token(query: &str) -> Option<String> {
    let rest: Vec<&str> = query
        .split('&')
        .filter(|pair| {
            !(*pair == TOKEN_QUERY_KEY || pair.starts_with(&format!("{TOKEN_QUERY_KEY}=")))
        })
        .filter(|pair| !pair.is_empty())
        .collect();
    (!rest.is_empty()).then(|| rest.join("&"))
}

/// What needs no session: the agent guides, and the DACTAL page with its
/// scripts — static files that run sandboxed (`embed.rs`), so a session
/// would be neither needed nor usable.
fn is_public(path: &str) -> bool {
    path == "/agent.md"
        || path.starts_with("/agent/")
        || path
            .strip_prefix('/')
            .is_some_and(|p| p.starts_with(crate::embed::PUBLIC_PREFIX))
}

fn is_api(path: &str) -> bool {
    path.starts_with("/api/")
}

pub async fn require_token(State(gate): State<Gate>, req: Request<Body>, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let public = is_public(&path);
    let auth = &gate.token;
    let credential = if public { None } else { auth.credential(&req) };
    let requester = match credential {
        None => gate.trusted_proxy.map(|p| p.requester(req.headers())),
        Some(_) => None,
    };
    let carrier = match (credential, requester) {
        (Some(Credential::Cookie), _) => Carrier::Cookie,
        (Some(_), _) => Carrier::Token,
        (None, Some(Requester::Owner)) => Carrier::ProxyOwner,
        (None, _) => Carrier::Nothing,
    };
    let host = request_host(&req);
    let verdict = fetch_policy(
        req.method(),
        host.as_deref(),
        req.headers(),
        carrier,
        public,
        gate.trusted_proxy.is_some().then_some(&gate.hosts),
    );
    if let Some(why) = verdict.refusal() {
        tracing::warn!(
            "refused {} {path} (Host {:?}, Sec-Fetch-Site {:?}): {why}",
            req.method(),
            host.as_deref().unwrap_or(""),
            req.headers()
                .get("sec-fetch-site")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
        );
        return forbidden(&path, why);
    }
    if public {
        return next.run(req).await;
    }

    match credential {
        None => match requester {
            Some(Requester::Owner) => next.run(req).await,
            Some(Requester::Visitor) => forbidden(
                &path,
                "this Datalib is only available to the workspace's owner",
            ),
            Some(Requester::Malformed) => {
                tracing::warn!(
                    "{} identity header the proxy would not have written on {} {path}; refused",
                    gate.trusted_proxy.map_or("", |p| p.as_str()),
                    req.method()
                );
                unauthorized(auth, &path)
            }
            None | Some(Requester::Unvouched) => unauthorized(auth, &path),
        },

        Some(Credential::Cookie) => next.run(req).await,

        // Token in the URL of a document request: hand back the cookie
        // and bounce to the same URL without it, so it doesn't survive
        // in history / bookmarks / `Referer`. `/api/*` is left alone —
        // a redirect would break the `fetch`/`EventSource` callers that
        // legitimately use `?token=`.
        Some(Credential::Query) if !is_api(&path) => {
            let target = match req.uri().query().and_then(query_without_token) {
                Some(q) => format!("{path}?{q}"),
                None => path,
            };
            let mut resp = Redirect::to(&target).into_response();
            resp.headers_mut()
                .append(header::SET_COOKIE, auth.cookie_header());
            resp
        }

        Some(Credential::Query) | Some(Credential::Presented) => {
            let api = is_api(&path);
            let mut resp = next.run(req).await;
            // Mint the session on document loads only. An API caller
            // that already sends the header has no use for a cookie,
            // and this keeps `Set-Cookie` off every JSON response.
            if !api {
                resp.headers_mut()
                    .append(header::SET_COOKIE, auth.cookie_header());
            }
            resp
        }
    }
}

fn unauthorized(auth: &ApiToken, path: &str) -> Response {
    let token_file = auth.token_file().display().to_string();
    if is_api(path) {
        return (
            StatusCode::UNAUTHORIZED,
            format!(
                "unauthorized: this endpoint requires the server's API token.\n\
                 Send it as `Authorization: Bearer <token>`; the running \
                 server's token is in\n  {token_file}\n"
            ),
        )
            .into_response();
    }
    // A browser landed here without a session — almost always a stale
    // tab from a previous run of the server (each run mints a fresh
    // token). Say so, and say what to do about it.
    refusal_page(
        StatusCode::UNAUTHORIZED,
        "Datalib — token required",
        &format!(
            "<h1>This browser isn't authenticated</h1>\
             <p>Datalib is running, but the local API requires a token — the same way \
             a Jupyter notebook server does. Without it, any web page you have open \
             could reach this server.</p>\
             <p>If this is a tab left over from an earlier run, the token has changed: \
             open the URL the launcher printed. Otherwise append the token to the URL \
             once and this browser gets a session cookie:</p>\
             <pre>{path}?token=$(cat {token_file})</pre>\
             <p>The running server's token is in <code>{token_file}</code>.</p>"
        ),
    )
}

fn forbidden(path: &str, why: &str) -> Response {
    if is_api(path) {
        return (StatusCode::FORBIDDEN, format!("forbidden: {why}\n")).into_response();
    }
    refusal_page(
        StatusCode::FORBIDDEN,
        "Datalib — not available",
        &format!("<h1>Not available</h1><p>Datalib refused this request: {why}.</p>"),
    )
}

fn refusal_page(status: StatusCode, title: &str, body: &str) -> Response {
    let html = format!(
        "<!doctype html>\n\
         <html lang=\"en\"><head><meta charset=\"utf-8\">\
         <title>{title}</title>\
         <style>body{{font:15px/1.55 system-ui,sans-serif;margin:8vh auto;max-width:44rem;\
         padding:0 1.5rem;color:#222}}code{{background:#f2f2f2;padding:.15em .35em;\
         border-radius:3px}}pre{{background:#f2f2f2;padding:.8rem 1rem;border-radius:5px;\
         overflow-x:auto}}@media(prefers-color-scheme:dark){{body{{background:#161616;\
         color:#ddd}}code,pre{{background:#242424}}}}</style></head><body>\
         {body}\
         </body></html>\n"
    );
    (
        status,
        [
            (header::CONTENT_TYPE, "text/html; charset=utf-8"),
            (
                header::CONTENT_SECURITY_POLICY,
                "default-src 'none'; style-src 'unsafe-inline'",
            ),
        ],
        html,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token() -> ApiToken {
        ApiToken::from_value("t0ken", Path::new("/tmp/root"))
    }

    #[test]
    fn minted_tokens_are_url_safe_and_long() {
        let t = ApiToken::mint(Path::new("/tmp/root")).unwrap();
        assert_eq!(t.value().len(), 64, "two simple-form uuids");
        assert!(is_url_safe(t.value()), "{}", t.value());
    }

    #[test]
    fn two_processes_get_two_cookie_names() {
        let a = ApiToken::from_value("aaaa", Path::new("/tmp/root"));
        let b = ApiToken::from_value("bbbb", Path::new("/tmp/root"));
        assert_ne!(a.0.cookie_name, b.0.cookie_name);
        assert!(a.0.cookie_name.starts_with("datalib_token_"));
    }

    #[test]
    fn matches_is_exact() {
        let t = token();
        assert!(t.matches("t0ken"));
        assert!(!t.matches("t0keN"));
        assert!(!t.matches("t0ken "));
        assert!(!t.matches("t0ke"));
        assert!(!t.matches(""));
    }

    #[test]
    fn query_token_is_found_in_any_position() {
        assert_eq!(query_token("token=abc"), Some("abc"));
        assert_eq!(query_token("a=1&token=abc&b=2"), Some("abc"));
        assert_eq!(query_token("a=1"), None);
        // Not a prefix match on some other key.
        assert_eq!(query_token("tokenish=abc"), None);
    }

    #[test]
    fn stripping_the_token_preserves_the_rest() {
        assert_eq!(query_without_token("token=abc"), None);
        assert_eq!(query_without_token("a=1&token=abc"), Some("a=1".into()));
        assert_eq!(
            query_without_token("token=abc&cols=xy&q=hi"),
            Some("cols=xy&q=hi".into())
        );
    }

    #[test]
    fn env_tokens_must_be_url_safe() {
        assert!(is_url_safe("abc-123_x.y~z"));
        assert!(!is_url_safe("has space"));
        assert!(!is_url_safe("percent%20"));
        assert!(!is_url_safe(""));
    }

    #[test]
    fn only_the_agent_guides_and_the_dactal_statics_are_public() {
        assert!(is_public("/agent.md"));
        assert!(is_public("/agent/cards.md"));
        assert!(is_public("/dactal/index.html"));
        assert!(is_public("/dactal/vendor/dactal.js"));
        assert!(!is_public("/dactalx"));
        assert!(!is_public("/api/health"));
        assert!(!is_public("/"));
        assert!(!is_public("/agentfoo"));
    }

    #[test]
    fn trusted_proxy_spellings_round_trip() {
        for proxy in <TrustedProxy as strum::VariantArray>::VARIANTS {
            assert_eq!(TrustedProxy::parse(proxy.as_str()), Some(*proxy));
        }
        assert_eq!(TrustedProxy::parse("imbue"), Some(TrustedProxy::Imbue));
        assert_eq!(TrustedProxy::parse("Imbue"), None);
        assert_eq!(TrustedProxy::parse(""), None);
        assert_eq!(TrustedProxy::parse("nginx"), None);
    }

    /// The header is read for what it says and nothing more: extra
    /// fields (the shared workspace's `user_id` and `email`) are fine,
    /// anything that is not a JSON object with a boolean `owner` is not.
    #[test]
    fn the_imbue_identity_header_names_the_owner_or_a_visitor() {
        let requester = |raw: &str| {
            let mut headers = HeaderMap::new();
            headers.insert("x-imbue-identity", HeaderValue::from_str(raw).unwrap());
            TrustedProxy::Imbue.requester(&headers)
        };
        assert_eq!(requester(r#"{"owner":true}"#), Requester::Owner);
        assert_eq!(
            requester(r#"{"owner":true,"user_id":"u1","email":"a@b.example"}"#),
            Requester::Owner
        );
        assert_eq!(
            requester(r#"{"owner":false,"user_id":"u2","email":"c@d.example"}"#),
            Requester::Visitor
        );
        for bad in ["", "true", "{}", r#"{"owner":"true"}"#, "owner=true", "{"] {
            assert_eq!(requester(bad), Requester::Malformed, "{bad:?}");
        }
        assert_eq!(
            TrustedProxy::Imbue.requester(&HeaderMap::new()),
            Requester::Unvouched
        );
    }

    type Case<'a> = (
        Method,
        Option<&'a str>,
        Option<&'a str>,
        Option<&'a str>,
        Carrier,
        bool,
        bool,
        Option<&'a str>,
        FetchVerdict,
    );

    /// The policy's case table: method × `Sec-Fetch-Site` × `Sec-Fetch-Mode`
    /// × `Origin` × credential × proxy mode × public route × `Host`.
    #[test]
    fn the_fetch_policy_case_table() {
        use Carrier::{Cookie, Nothing, ProxyOwner, Token};
        use FetchVerdict::{
            Allow, CookieWriteFromElsewhere as NoOrigin, ForeignHost as Host,
            FromAnotherSite as Site,
        };
        const H: Option<&str> = Some("127.0.0.1:8731");
        let hosts = AllowedHosts::parse("datalib.example, *.share.example").unwrap();
        #[rustfmt::skip]
        let table: &[Case] = &[
            // method      site                 mode              origin                          carrier     proxy  public host                              verdict
            // The app's own requests, and the address bar.
            (Method::GET,  Some("same-origin"), Some("cors"),     None,                           Cookie,     false, false, H,                                 Allow),
            (Method::POST, Some("same-origin"), Some("cors"),     Some("http://127.0.0.1:8731"),  Cookie,     false, false, H,                                 Allow),
            (Method::GET,  Some("none"),        Some("navigate"), None,                           Cookie,     false, false, H,                                 Allow),
            (Method::PUT,  Some("none"),        Some("cors"),     None,                           Token,      false, false, H,                                 Allow),
            // A navigation from anywhere, to any path.
            (Method::GET,  Some("cross-site"),  Some("navigate"), None,                           Cookie,     false, false, H,                                 Allow),
            (Method::HEAD, Some("same-site"),   Some("navigate"), None,                           ProxyOwner, true , false, Some("localhost:8731"),            Allow),
            (Method::GET,  Some("cross-site"),  Some("navigate"), None,                           Nothing,    false, false, H,                                 Allow),
            // Anything else from another site or another port: refused,
            // the token and the proxy's word included.
            (Method::POST, Some("same-site"),   Some("navigate"), None,                           Cookie,     false, false, H,                                 Site),
            (Method::POST, Some("same-site"),   Some("no-cors"),  Some("http://127.0.0.1:9000"),  Cookie,     false, false, H,                                 Site),
            (Method::GET,  Some("cross-site"),  Some("no-cors"),  None,                           Cookie,     false, false, H,                                 Site),
            (Method::GET,  Some("same-site"),   Some("cors"),     None,                           Token,      false, false, H,                                 Site),
            (Method::DELETE, Some("cross-site"), Some("cors"),    None,                           ProxyOwner, true , false, Some("localhost:8731"),            Site),
            (Method::POST, Some("cross-site"),  None,             None,                           Nothing,    false, false, H,                                 Site),
            (Method::POST, Some("bogus"),       Some("cors"),     None,                           Cookie,     false, false, H,                                 Site),
            // No fetch metadata: the token and the proxy's word pass, a
            // cookie-only write needs this server's Origin.
            (Method::POST, None,                None,             None,                           Token,      false, false, H,                                 Allow),
            (Method::POST, None,                None,             None,                           ProxyOwner, true , false, Some("localhost:8731"),            Allow),
            (Method::GET,  None,                None,             None,                           Cookie,     false, false, H,                                 Allow),
            (Method::POST, None,                None,             Some("http://127.0.0.1:8731"),  Cookie,     false, false, H,                                 Allow),
            (Method::POST, None,                None,             None,                           Cookie,     false, false, H,                                 NoOrigin),
            (Method::POST, None,                None,             Some("http://127.0.0.1:9000"),  Cookie,     false, false, H,                                 NoOrigin),
            (Method::PUT,  None,                None,             Some("http://evil.example"),    Cookie,     false, false, H,                                 NoOrigin),
            (Method::POST, None,                None,             Some("http://127.0.0.1:8731"),  Cookie,     false, false, None,                              NoOrigin),
            // Host, in proxy mode: loopback, names under localhost and IP
            // literals at any port, the configured names, and no Host at
            // all (not a browser).
            (Method::GET,  Some("same-origin"), Some("cors"),     None,                           ProxyOwner, true,  false, Some("localhost:5173"),            Allow),
            (Method::GET,  Some("same-origin"), Some("cors"),     None,                           ProxyOwner, true,  false, Some("datalib-x7k9.host-ab12.localhost:8421"), Allow),
            (Method::GET,  Some("same-origin"), Some("cors"),     None,                           ProxyOwner, true,  false, Some("[::1]:8731"),                Allow),
            (Method::GET,  Some("same-origin"), Some("cors"),     None,                           ProxyOwner, true,  false, Some("192.168.1.20:8731"),         Allow),
            (Method::GET,  Some("same-origin"), Some("cors"),     None,                           ProxyOwner, true,  false, Some("datalib-x7k9.share.example"), Allow),
            (Method::GET,  Some("same-origin"), Some("cors"),     None,                           ProxyOwner, true,  false, Some("DATALIB.example:443"),       Allow),
            (Method::GET,  Some("same-origin"), Some("cors"),     None,                           ProxyOwner, true,  false, None,                              Allow),
            (Method::GET,  Some("same-origin"), Some("cors"),     None,                           ProxyOwner, true,  false, Some("rebind.evil.example:8731"),  Host),
            (Method::GET,  Some("same-origin"), Some("cors"),     None,                           Cookie,     true,  false, Some("share.example"),             Host),
            (Method::GET,  Some("same-origin"), Some("cors"),     None,                           Token,      true,  false, Some("localhost.evil.example"),    Host),
            (Method::GET,  Some("same-origin"), Some("cors"),     None,                           Nothing,    true,  false, Some("127.0.0.1:abc"),             Host),
            (Method::GET,  Some("same-origin"), Some("cors"),     None,                           ProxyOwner, true,  false, Some(""),                          Host),
            // Outside proxy mode a rebound page has no credential to send,
            // so Host is not checked.
            (Method::GET,  Some("same-origin"), Some("cors"),     None,                           Cookie,     false, false, Some("rebind.evil.example:8731"),  Allow),
            (Method::GET,  Some("same-origin"), Some("cors"),     None,                           Token,      false, false, Some("datalib-x7k9.share.example"), Allow),
            (Method::GET,  Some("same-origin"), Some("cors"),     None,                           Nothing,    false, false, Some(""),                          Allow),
            // A public route needs a right Host, in proxy mode, and nothing else.
            (Method::GET,  Some("cross-site"),  Some("no-cors"),  None,                           Nothing,    false, true,  H,                                 Allow),
            (Method::GET,  Some("cross-site"),  Some("no-cors"),  None,                           Nothing,    true,  true,  Some("rebind.evil.example"),       Host),
            (Method::GET,  Some("cross-site"),  Some("no-cors"),  None,                           Nothing,    false, true,  Some("rebind.evil.example"),       Allow),
        ];
        for (i, (method, site, mode, origin, carrier, proxy, public, host, want)) in
            table.iter().enumerate()
        {
            let mut headers = HeaderMap::new();
            for (name, value) in [
                ("sec-fetch-site", site),
                ("sec-fetch-mode", mode),
                ("origin", origin),
            ] {
                if let Some(value) = value {
                    headers.insert(name, HeaderValue::from_str(value).unwrap());
                }
            }
            let proxy_hosts = proxy.then_some(&hosts);
            let got = fetch_policy(method, *host, &headers, *carrier, *public, proxy_hosts);
            assert_eq!(got, *want, "row {i}: {:?}", table[i]);
        }
    }

    #[test]
    fn allowed_hosts_are_names_or_wildcard_domains() {
        assert_eq!(AllowedHosts::parse("").unwrap(), AllowedHosts::default());
        assert!(AllowedHosts::parse(" a.example , *.b.example ,").is_ok());
        for bad in [
            "http://a.example",
            "a.example:8731",
            "*",
            "*.",
            "a b",
            "a/b",
        ] {
            assert!(AllowedHosts::parse(bad).is_err(), "{bad:?}");
        }
        let none = AllowedHosts::default();
        assert!(none.admits("localhost"));
        assert!(none.admits("datalib-x7k9q2w1.host-0123abcd.localhost:8421"));
        assert!(!none.admits(".localhost"));
        assert!(!none.admits("localhost.evil.example"));
        assert!(none.admits("127.0.0.1"));
        assert!(none.admits("[::1]"));
        assert!(!none.admits("[::1"));
        assert!(!none.admits("[not-v6]:80"));
        assert!(!none.admits("example.com"));
        assert!(!none.admits(":8731"));
    }

    #[test]
    fn token_file_lands_under_system_state() {
        let t = token();
        assert!(
            t.token_file().ends_with("system/api-token"),
            "{:?}",
            t.token_file()
        );
    }

    #[test]
    fn written_token_file_is_owner_only() {
        let dir = tempfile::tempdir().unwrap();
        let t = ApiToken::from_value("s3cret", dir.path());
        t.write_token_file().unwrap();
        assert_eq!(std::fs::read_to_string(t.token_file()).unwrap(), "s3cret");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(t.token_file())
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(
                mode & 0o777,
                0o600,
                "token file must not be readable by others"
            );
        }
    }
}
