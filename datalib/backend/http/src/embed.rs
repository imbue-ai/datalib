//! Embeds the Vite-built UI into the binary via `rust-embed`, then
//! serves it through axum — with the response headers that keep a
//! document served from this origin from running anything it should
//! not.

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

use std::sync::Arc;

use crate::AppState;

#[derive(RustEmbed)]
#[folder = "$DATALIB_UI_DIST"]
struct UiAssets;

macro_rules! app_csp_directives {
    () => {
        "default-src 'self'; \
         script-src 'self'; \
         style-src 'self' 'unsafe-inline'; \
         img-src 'self' data: blob:; \
         media-src 'self' data: blob:; \
         font-src 'self' data:; \
         connect-src 'self' ipc: http://ipc.localhost; \
         frame-src 'self'; \
         worker-src 'self' blob:; \
         object-src 'none'; \
         base-uri 'none'; \
         form-action 'self'"
    };
}

/// The app page's Content-Security-Policy: the second layer behind
/// DOMPurify. A message body that gets past the sanitizer still cannot
/// run as a script, because only the bundle's own files may — no inline
/// script, no `javascript:` URL, no foreign origin. No `'unsafe-eval'`
/// either: card source is parsed as a call, never evaluated
/// (docs/dev/cards.md § "Card source"), so a string that reaches an
/// evaluator by mistake is refused by the browser.
///
/// The escape hatches, each load-bearing:
/// - `style-src 'unsafe-inline'`: the custom elements put `<style>` in
///   their shadow roots, and the renderers emit `style=`. Inline style
///   is not a code path.
/// - `img-src`/`media-src` `'self' data: blob:` and **no remote host**:
///   a rendered email or chat may reference a remote image, and loading
///   it tells the sender's server your address and the moment you
///   opened the message — a tracking pixel is exactly that. The
///   sanitizer (`ui/src/cards/sanitize.ts`) strips remote references
///   before they reach the DOM and offers to load them; this line is
///   the guarantee behind it, for anything the sanitizer misses. A
///   reference the person chooses to load has to come from this
///   origin, then — a server-side fetch into a store (issue #648).
/// - `connect-src ipc: http://ipc.localhost`: Tauri's IPC. The desktop
///   shell loads this page from the server as a remote URL, so Tauri
///   does not rewrite the policy the way it would for a page it serves
///   itself, and its `invoke` is a `fetch` to those origins.
/// - `frame-src 'self'`: the DACTAL card and a document's body are
///   same-origin iframes — which is why every document this origin
///   serves as data gets a sandbox policy of its own
///   ([`DATA_DOCUMENT_CSP`]).
/// - `frame-ancestors`, `'none'` unless [`FrameAncestors`] names who may
///   frame the app: otherwise a page could lay an invisible copy of it
///   under its own buttons. A page on another port of the same host
///   would carry the session, since `SameSite` ignores the port.
pub const APP_CSP: &str = concat!(app_csp_directives!(), "; frame-ancestors 'none'");

/// Env naming who may frame the app page, as the source list of a CSP
/// `frame-ancestors` directive. Unset or empty, nobody may. Imbue
/// Studio's launcher sets the origins its desktop app and its share
/// chrome frame the app from. A proxy in front may append a
/// `frame-ancestors` of its own; the browser enforces both.
pub const FRAME_ANCESTORS_ENV: &str = "DATALIB_FRAME_ANCESTORS";

/// Who may frame the app page, and the page's whole policy built from
/// it. Cheap to clone.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FrameAncestors {
    sources: String,
    app_csp: Arc<str>,
}

impl Default for FrameAncestors {
    fn default() -> Self {
        Self {
            sources: "'none'".to_string(),
            app_csp: APP_CSP.into(),
        }
    }
}

impl FrameAncestors {
    /// A space-separated list of CSP sources: `'self'`, a scheme
    /// (`https:`), or a host with an optional scheme and port, where the
    /// host may start with `*.` and the port may be `*`
    /// (`https://*.localhost:*`). `'none'` stands alone. Anything else,
    /// a `;` that would end the directive above all, is refused.
    pub fn parse(list: &str) -> Result<Self, String> {
        let sources: Vec<&str> = list.split_whitespace().collect();
        if sources.is_empty() || sources == ["'none'"] {
            return Ok(Self::default());
        }
        if let Some(bad) = sources.iter().find(|s| !is_frame_source(s)) {
            return Err(format!("{bad:?} is not a frame-ancestors source"));
        }
        let sources = sources.join(" ");
        let app_csp = format!("{}; frame-ancestors {sources}", app_csp_directives!()).into();
        Ok(Self { sources, app_csp })
    }

    pub fn from_env() -> anyhow::Result<Self> {
        let raw = std::env::var(FRAME_ANCESTORS_ENV).unwrap_or_default();
        Self::parse(&raw).map_err(|e| anyhow::anyhow!("{FRAME_ANCESTORS_ENV}: {e}"))
    }

    pub fn sources(&self) -> &str {
        &self.sources
    }

    fn app_csp(&self) -> HeaderValue {
        HeaderValue::from_str(&self.app_csp).expect("sources are checked ASCII")
    }
}

fn is_frame_source(source: &str) -> bool {
    if source == "'self'" {
        return true;
    }
    let is_scheme = |s: &str| {
        s.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
    };
    if let Some(scheme) = source.strip_suffix(':') {
        return is_scheme(scheme);
    }
    let rest = match source.split_once("://") {
        Some((scheme, rest)) if is_scheme(scheme) => rest,
        Some(_) => return false,
        None => source,
    };
    let (host, port) = match rest.rsplit_once(':') {
        Some((host, port)) => (host, Some(port)),
        None => (rest, None),
    };
    let port_ok =
        port.is_none_or(|p| p == "*" || (!p.is_empty() && p.bytes().all(|b| b.is_ascii_digit())));
    let host = host.strip_prefix("*.").unwrap_or(host);
    let label_ok =
        |l: &str| !l.is_empty() && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
    port_ok && (host == "*" || host.split('.').all(label_ok))
}

/// What a document this origin serves as *data* — not the app — may do:
/// an attachment out of a render tree, a fetched remote SVG. A sender
/// wrote them, so they run no script at all and load nothing from
/// outside — not even a read receipt. `sandbox` puts the document in an
/// opaque origin: no cookie, no same-origin `fetch` to `/api/*`, no way
/// to navigate the window it sits in. It is sent as a header because
/// `sandbox` is ignored in a `<meta>` policy.
pub const DATA_DOCUMENT_CSP: &str = "sandbox; default-src 'none'; style-src 'unsafe-inline'; \
     img-src 'self' data:; media-src 'self' data:; font-src data:; \
     base-uri 'none'; form-action 'none'";

/// The DACTAL page (`/dactal/`) runs its own scripts in the sandbox. Its
/// `<meta>` policy holds the rest (`ui/public/dactal/index.html`) — a
/// header naming sources would intersect with it.
pub const DACTAL_SANDBOX_CSP: &str = "sandbox allow-scripts allow-downloads";

/// The types data keeps when served: ones a browser shows rather than
/// runs, and never takes as a script or a stylesheet. A list of the
/// types that *do* run would never be complete — browsers render
/// `text/xsl` as a page, and take one entry of a comma list — so this is
/// the only list, and it names what is safe.
const SHOWN_KINDS: &[&str] = &["image/", "audio/", "video/"];
const SHOWN_TYPES: &[&str] = &[
    "application/json",
    "application/pdf",
    "text/csv",
    "text/html",
    "text/markdown",
    "text/plain",
];

/// The headers for bytes this origin serves as data — anything a sender,
/// an upstream service or an applet chose: a blob, a fetched remote
/// image, an applet's answer. Every one runs under [`DATA_DOCUMENT_CSP`],
/// whatever type it names, except a PDF, whose viewer the sandbox turns
/// off and which runs in an origin of its own. A type that is not one of
/// [`SHOWN_TYPES`] goes out as `application/octet-stream`, which under
/// `nosniff` no page loads as script.
pub fn set_data_headers(headers: &mut header::HeaderMap, declared: &str) {
    let shown = mime_essence(declared).filter(|m| {
        SHOWN_KINDS.iter().any(|k| m.starts_with(k)) || SHOWN_TYPES.contains(&m.as_str())
    });
    let served = match (&shown, charset(declared)) {
        (Some(mime), Some(cs)) => format!("{mime}; charset={cs}"),
        (Some(mime), None) => mime.clone(),
        (None, _) => "application/octet-stream".to_string(),
    };
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&served).expect("built from token characters"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if shown.as_deref() != Some("application/pdf") {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(DATA_DOCUMENT_CSP),
        );
    }
}

/// `type/subtype`, lowercased, when `content_type` names exactly one
/// type; `None` for a comma list or anything else malformed.
pub fn mime_essence(content_type: &str) -> Option<String> {
    let essence = content_type.split(';').next()?.trim().to_ascii_lowercase();
    let (kind, sub) = essence.split_once('/')?;
    (is_token(kind) && is_token(sub)).then_some(essence)
}

fn charset(content_type: &str) -> Option<&str> {
    content_type
        .split(';')
        .skip(1)
        .find_map(|param| {
            let (key, value) = param.split_once('=')?;
            key.trim()
                .eq_ignore_ascii_case("charset")
                .then(|| value.trim().trim_matches('"'))
        })
        .filter(|v| is_token(v))
}

fn is_token(s: &str) -> bool {
    !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"!#$%&'*+-.^_`|~".contains(&b))
}

/// Files the token gate lets through unauthenticated: the DACTAL page
/// and its scripts. They are static and identical on every install,
/// and the page runs sandboxed (see `serve_ui`), so it needs no
/// session — and, being sandboxed, could not use one: an opaque origin
/// sends no cookie with a module load.
pub const PUBLIC_PREFIX: &str = "dactal/";

pub async fn serve_ui(State(state): State<AppState>, uri: Uri) -> Response {
    let frame_ancestors = &state.frame_ancestors;
    let path = uri.path().trim_start_matches('/');
    if path.is_empty() {
        return serve_index(frame_ancestors);
    }
    match UiAssets::get(path) {
        Some(content) => asset_response(path, content, frame_ancestors),
        // A public path names a file or nothing: the SPA fallback would
        // hand the app shell to a caller with no session.
        None if path.starts_with(PUBLIC_PREFIX) => StatusCode::NOT_FOUND.into_response(),
        // An API call that names no endpoint must not read as a success.
        None if path.starts_with("api/") => {
            (StatusCode::NOT_FOUND, format!("no endpoint /{path}")).into_response()
        }
        // SPA fallback — let the client router handle unknown routes.
        None => serve_index(frame_ancestors),
    }
}

fn serve_index(frame_ancestors: &FrameAncestors) -> Response {
    match UiAssets::get("index.html") {
        Some(c) => asset_response("index.html", c, frame_ancestors),
        // Built without a UI bundle present. Surface a clear error
        // rather than a confusing 404 — the right fix is to populate
        // DATALIB_UI_DIST and rebuild.
        None => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "UI bundle not embedded in this binary",
        )
            .into_response(),
    }
}

fn asset_response(
    path: &str,
    content: rust_embed::EmbeddedFile,
    frame_ancestors: &FrameAncestors,
) -> Response {
    let mime = mime_guess::from_path(path).first_or_octet_stream();
    let mut resp = Response::new(Body::from(content.data));
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime.as_ref()).unwrap_or(HeaderValue::from_static("text/plain")),
    );
    // Cache policy: Vite content-hashes everything under `assets/`, so
    // those are safe to cache forever (a content change yields a new
    // filename). The entry `index.html` is NOT hashed and points at the
    // current chunk names, so it must be revalidated on every load —
    // otherwise a reload can serve a whole stale app (old index.html +
    // its old chunks) from disk cache and the UI silently runs an old
    // bundle. `no-cache` (revalidate, not "never store") is right for
    // the entry document and the SPA fallback.
    let cache = if path.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if path == "index.html" {
        headers.insert(header::CONTENT_SECURITY_POLICY, frame_ancestors.app_csp());
    }
    if path.starts_with(PUBLIC_PREFIX) {
        // The sandboxed page loads `main.js` as a module, and a module
        // load from an opaque origin is a CORS request.
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        );
        if mime.essence_str() == "text/html" {
            headers.insert(
                header::CONTENT_SECURITY_POLICY,
                HeaderValue::from_static(DACTAL_SANDBOX_CSP),
            );
        }
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_ancestors_takes_csp_sources_and_nothing_else() {
        let studio = "http://localhost:* http://127.0.0.1:* https://*.localhost:* \
                      https://*.personal-imbue.com https://studio.imbue.com";
        let parsed = FrameAncestors::parse(studio).unwrap();
        assert_eq!(
            parsed.sources(),
            studio.split_whitespace().collect::<Vec<_>>().join(" ")
        );
        assert!(parsed
            .app_csp
            .ends_with(&format!("; frame-ancestors {}", parsed.sources())));
        assert!(!parsed.app_csp.contains("frame-ancestors 'none'"));
        for ok in [
            "'self'",
            "https:",
            "*",
            "localhost:8421",
            "https://a-b.example:443",
        ] {
            assert!(FrameAncestors::parse(ok).is_ok(), "{ok}");
        }
        for none in ["", "  ", "'none'"] {
            assert_eq!(
                FrameAncestors::parse(none).unwrap(),
                FrameAncestors::default()
            );
        }
        for bad in [
            "https://a.example; script-src *",
            "'none' https://a.example",
            "'unsafe-inline'",
            "https://a.example/path",
            "https://a.example,https://b.example",
            "https://a.example:80x",
            "*.",
        ] {
            assert!(FrameAncestors::parse(bad).is_err(), "{bad}");
        }
    }

    fn served(declared: &str) -> (String, Option<String>) {
        let mut headers = header::HeaderMap::new();
        set_data_headers(&mut headers, declared);
        let get = |name| {
            headers
                .get(name)
                .map(|v: &HeaderValue| v.to_str().unwrap().to_string())
        };
        assert_eq!(
            get(header::X_CONTENT_TYPE_OPTIONS).as_deref(),
            Some("nosniff")
        );
        (
            get(header::CONTENT_TYPE).unwrap(),
            get(header::CONTENT_SECURITY_POLICY),
        )
    }

    /// Browsers render `text/xsl` as a page and take one entry of a comma
    /// list; a check that matched dangerous types let both run script in
    /// the app's origin. Data is sandboxed whatever it is called.
    #[test]
    fn data_is_sandboxed_whatever_type_it_names() {
        for declared in [
            "text/xsl; charset=utf-8",
            "text/plain,text/html",
            "text/html, application/x-foo",
            "image/png, text/html",
            "text/html",
            "image/svg+xml",
            "image/png",
            "application/json",
            "application/x-never-heard-of-it",
            "",
            "not a type",
        ] {
            assert_eq!(
                served(declared).1.as_deref(),
                Some(DATA_DOCUMENT_CSP),
                "{declared:?}"
            );
        }
    }

    #[test]
    fn data_keeps_its_type_only_when_it_is_one_shown_type() {
        for (declared, expected) in [
            ("image/png", "image/png"),
            ("IMAGE/JPEG", "image/jpeg"),
            ("video/mp4", "video/mp4"),
            ("text/html; charset=utf-8", "text/html; charset=utf-8"),
            ("text/plain; Charset=\"UTF-8\"", "text/plain; charset=UTF-8"),
            ("text/plain; charset=utf-8, text/html", "text/plain"),
            ("application/json", "application/json"),
            ("text/xsl; charset=utf-8", "application/octet-stream"),
            ("text/plain,text/html", "application/octet-stream"),
            ("image/png, text/html", "application/octet-stream"),
            ("text/javascript", "application/octet-stream"),
            ("application/wasm", "application/octet-stream"),
            ("text/css", "application/octet-stream"),
            ("", "application/octet-stream"),
        ] {
            assert_eq!(served(declared).0, expected, "{declared:?}");
        }
    }

    /// The sandbox turns off the browser's PDF viewer, which runs in an
    /// origin of its own.
    #[test]
    fn a_pdf_is_the_one_shown_type_without_the_sandbox() {
        assert_eq!(
            served("application/pdf"),
            ("application/pdf".to_string(), None)
        );
        assert!(served("application/pdf,text/html").1.is_some());
    }

    fn directive<'a>(csp: &'a str, name: &str) -> Option<&'a str> {
        csp.split(';')
            .map(str::trim)
            .find(|d| d == &name || d.starts_with(&format!("{name} ")))
    }

    /// A sender's HTML framed in a document view must not run, and must
    /// not reach the network even if it could — a read receipt is the
    /// attack (audit 2026-10-02 finding 1).
    #[test]
    fn data_documents_run_nothing_and_reach_nothing() {
        let csp = DATA_DOCUMENT_CSP;
        assert_eq!(directive(csp, "sandbox"), Some("sandbox"), "{csp}");
        assert_eq!(directive(csp, "default-src"), Some("default-src 'none'"));
        assert!(directive(csp, "script-src").is_none());
        assert!(directive(csp, "connect-src").is_none());
        for d in csp.split(';') {
            assert!(!d.contains("http"), "no remote origin in {d:?}");
        }
    }
}
