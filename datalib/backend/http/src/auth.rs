//! Per-process API token — the front door's only authentication.
//!
//! The server binds loopback, but loopback is not a security boundary against
//! the *browser*: any page the user has open can `fetch()`
//! `http://127.0.0.1:8731/…`. With no auth that page could `PUT /api/config` a
//! step whose `command` is an arbitrary shell string and then run it.
//!
//! So one random token is minted per process and required on every route
//! that reads or changes data, accepted as `Authorization: Bearer`,
//! `X-Datalib-Token` or `?token=`. The app shell (the UI bundle, which holds
//! no data), the agent guides and component modules are public; see
//! [`crate::router`] for which routes sit behind [`require_token`].
//!
//! The server sets no cookie. The page keeps the token in `localStorage`,
//! taken from the launch URL's `?token=` (`ui/src/apiToken.ts`), and sends it
//! as a header. Nothing the browser attaches by itself proves anything, so a
//! page on another origin, or on another port of this host, has nothing to
//! send: `localStorage` is per origin, the port included, and the server
//! never echoes the token. A URL that the browser loads without script (an
//! image, a media file, an `EventSource`) carries `?token=` instead.
//!
//! Behind a reverse proxy that authenticates every request itself and frames
//! the app in its own page, `frame-ancestors 'none'` would blank that frame.
//! `$DATALIB_TRUSTED_PROXY`
//! names such a proxy ([`TrustedProxy`]). Behind it, a request whose identity
//! header says the requester is the owner is let in without the token, a
//! request it marks as a visitor is refused, and the page's policy leaves
//! `frame-ancestors` to the proxy (`embed::app_csp`). The token keeps
//! working for everything else — an agent's `curl` on the same host — and a
//! request that carries neither is refused as before.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderMap, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
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

/// A state-changing request the proxy vouched for but a page on another
/// origin made the browser send. The proxy stamps the owner's identity on
/// everything it forwards, a sibling app's `POST` included; the browser's
/// own `Sec-Fetch-Site` tells that from the app's own request. Absent on a
/// non-browser client, which the proxy has already authenticated.
fn is_cross_site_write(method: &Method, headers: &HeaderMap) -> bool {
    if method.is_safe() {
        return false;
    }
    match headers.get("sec-fetch-site").and_then(|v| v.to_str().ok()) {
        None | Some("same-origin") | Some("none") => false,
        Some(_) => true,
    }
}

/// What [`require_token`] is configured with: the token, and the proxy
/// whose word is taken in its place, if any.
#[derive(Clone)]
pub struct Gate {
    pub token: ApiToken,
    pub trusted_proxy: Option<TrustedProxy>,
}

fn is_url_safe(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'))
}

/// The server's token and the file it's published in. Cheap to clone (one
/// `Arc`).
#[derive(Clone)]
pub struct ApiToken(Arc<Inner>);

struct Inner {
    value: String,
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
        Self(Arc::new(Inner {
            value: value.into(),
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

    fn presented(&self, req: &Request<Body>) -> bool {
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
                return true;
            }
        }

        if let Some(v) = headers.get(TOKEN_HEADER).and_then(|v| v.to_str().ok()) {
            if self.matches(v.trim()) {
                return true;
            }
        }

        req.uri()
            .query()
            .and_then(query_token)
            .is_some_and(|v| self.matches(v))
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

fn is_api(path: &str) -> bool {
    path.starts_with("/api/")
}

pub async fn require_token(State(gate): State<Gate>, req: Request<Body>, next: Next) -> Response {
    let path = req.uri().path().to_string();
    let auth = &gate.token;
    if auth.presented(&req) {
        return next.run(req).await;
    }
    match gate.trusted_proxy.map(|p| p.requester(req.headers())) {
        Some(Requester::Owner) if is_cross_site_write(req.method(), req.headers()) => {
            forbidden(&path, "a page on another origin made this request")
        }
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
             open the URL the launcher printed. Otherwise append the token to the URL:</p>\
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
    use axum::http::HeaderValue;

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

    #[test]
    fn a_write_from_another_origin_is_told_by_sec_fetch_site() {
        let with = |site: Option<&str>| {
            let mut headers = HeaderMap::new();
            if let Some(site) = site {
                headers.insert("sec-fetch-site", HeaderValue::from_str(site).unwrap());
            }
            headers
        };
        // The browser's own request from the app page, and a non-browser
        // client that sends no fetch metadata.
        assert!(!is_cross_site_write(
            &Method::POST,
            &with(Some("same-origin"))
        ));
        assert!(!is_cross_site_write(&Method::PUT, &with(Some("none"))));
        assert!(!is_cross_site_write(&Method::POST, &with(None)));
        // A sibling app on the same site, and a foreign site.
        assert!(is_cross_site_write(&Method::POST, &with(Some("same-site"))));
        assert!(is_cross_site_write(
            &Method::DELETE,
            &with(Some("cross-site"))
        ));
        // Reads are never writes, wherever they come from.
        assert!(!is_cross_site_write(
            &Method::GET,
            &with(Some("cross-site"))
        ));
        assert!(!is_cross_site_write(
            &Method::HEAD,
            &with(Some("same-site"))
        ));
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
