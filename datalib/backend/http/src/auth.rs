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
//! is not: a page on `evil.com` can neither read ours (`HttpOnly`) nor make
//! the browser send it (`SameSite=Lax`), and DNS rebinding fails closed,
//! whereas a token in the served HTML is readable by anything that can read
//! the page.

use axum::body::Body;
use axum::extract::{Request, State};
use axum::http::{header, HeaderValue, StatusCode};
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

fn query_without_token(query: &str) -> Option<String> {
    let rest: Vec<&str> = query
        .split('&')
        .filter(|pair| {
            !(*pair == TOKEN_QUERY_KEY || pair.starts_with(&format!("{TOKEN_QUERY_KEY}=")))
        })
        .filter(|pair| !pair.is_empty())
        .collect();
    (!rest.is_empty()).then(|| rest.join("&"))
}

fn is_public(path: &str) -> bool {
    path == "/agent.md" || path.starts_with("/agent/")
}

fn is_api(path: &str) -> bool {
    path.starts_with("/api/")
}

pub async fn require_token(
    State(auth): State<ApiToken>,
    req: Request<Body>,
    next: Next,
) -> Response {
    let path = req.uri().path().to_string();
    if is_public(&path) {
        return next.run(req).await;
    }

    match auth.credential(&req) {
        None => unauthorized(&auth, &path),

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
    let html = format!(
        "<!doctype html>\n\
         <html lang=\"en\"><head><meta charset=\"utf-8\">\
         <title>Datalib — token required</title>\
         <style>body{{font:15px/1.55 system-ui,sans-serif;margin:8vh auto;max-width:44rem;\
         padding:0 1.5rem;color:#222}}code{{background:#f2f2f2;padding:.15em .35em;\
         border-radius:3px}}pre{{background:#f2f2f2;padding:.8rem 1rem;border-radius:5px;\
         overflow-x:auto}}@media(prefers-color-scheme:dark){{body{{background:#161616;\
         color:#ddd}}code,pre{{background:#242424}}}}</style></head><body>\
         <h1>This browser isn't authenticated</h1>\
         <p>Datalib is running, but the local API requires a token — the same way \
         a Jupyter notebook server does. Without it, any web page you have open \
         could reach this server.</p>\
         <p>If this is a tab left over from an earlier run, the token has changed: \
         open the URL the launcher printed. Otherwise append the token to the URL \
         once and this browser gets a session cookie:</p>\
         <pre>{path}?token=$(cat {token_file})</pre>\
         <p>The running server's token is in <code>{token_file}</code>.</p>\
         </body></html>\n"
    );
    (
        StatusCode::UNAUTHORIZED,
        [(header::CONTENT_TYPE, "text/html; charset=utf-8")],
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
    fn only_the_agent_guides_are_public() {
        assert!(is_public("/agent.md"));
        assert!(is_public("/agent/cards.md"));
        assert!(!is_public("/api/health"));
        assert!(!is_public("/"));
        assert!(!is_public("/agentfoo"));
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
