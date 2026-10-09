//! Embeds the Vite-built UI into the binary via `rust-embed`, then
//! serves it through axum — with the response headers that keep a
//! document served from this origin from running anything it should
//! not.

use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "$DATALIB_UI_DIST"]
struct UiAssets;

/// The app page's Content-Security-Policy: the second layer behind
/// DOMPurify. A message body that gets past the sanitizer still cannot
/// run as a script, because only the bundle's own files may — no inline
/// script, no `javascript:` URL, no foreign origin.
///
/// The escape hatches, each load-bearing:
/// - `'unsafe-eval'`: a card's source is evaluated with `new Function`
///   (docs/dev/cards.md). It permits `eval` of strings the page already
///   has; it does not let an injected `<script>` element run.
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
/// - `frame-src 'self'`: the DACTAL card and the plot pages are
///   same-origin iframes — which is why every document this origin
///   serves as data gets a sandbox policy of its own ([`DocumentKind`]).
pub const APP_CSP: &str = "default-src 'self'; \
    script-src 'self' 'unsafe-eval'; \
    style-src 'self' 'unsafe-inline'; \
    img-src 'self' data: blob:; \
    media-src 'self' data: blob:; \
    font-src 'self' data:; \
    connect-src 'self' ipc: http://ipc.localhost; \
    frame-src 'self'; \
    worker-src 'self' blob:; \
    object-src 'none'; \
    base-uri 'none'; \
    form-action 'self'; \
    frame-ancestors 'none'";

/// What a document this origin serves as *data* — not the app — may do.
/// Every policy starts with `sandbox`, which puts the document in an
/// opaque origin: no cookie, no same-origin `fetch` to `/api/*`, no way
/// to navigate the window it sits in. It is sent as a header because
/// `sandbox` is ignored in a `<meta>` policy.
///
/// The kind is named by whoever knows what the bytes are; anything not
/// named is [`DocumentKind::Data`], the policy that runs nothing.
#[derive(
    Debug,
    Clone,
    Copy,
    PartialEq,
    Eq,
    Default,
    strum::EnumString,
    strum::IntoStaticStr,
    strum::VariantArray,
)]
#[strum(serialize_all = "snake_case")]
pub enum DocumentKind {
    /// Bytes from upstream: an attachment out of a render tree, a fetched
    /// remote SVG. A sender wrote them, so they run no script at all and
    /// load nothing from outside — not even a read receipt.
    #[default]
    Data,
    /// A Plotly page a time-series renderer wrote (`plots/*.html`). It
    /// needs its own inline script and Plotly from the pinned CDN
    /// (`timeseries_render::plot`; `scattergl` compiles shaders through
    /// `new Function`), and nothing else: `connect-src 'none'`.
    Plot,
}

/// The DACTAL page (`/dactal/`) runs its own scripts in the sandbox. Its
/// `<meta>` policy holds the rest (`ui/public/dactal/index.html`) — a
/// header naming sources would intersect with it — and it is not a kind
/// an applet can ask for.
pub const DACTAL_SANDBOX_CSP: &str = "sandbox allow-scripts allow-downloads";

impl DocumentKind {
    pub fn parse(s: &str) -> Option<Self> {
        s.parse().ok()
    }

    pub fn csp(self) -> &'static str {
        match self {
            DocumentKind::Data => {
                "sandbox; default-src 'none'; style-src 'unsafe-inline'; \
                 img-src 'self' data:; media-src 'self' data:; font-src data:; \
                 base-uri 'none'; form-action 'none'"
            }
            DocumentKind::Plot => {
                "sandbox allow-scripts allow-downloads; default-src 'none'; \
                 script-src 'unsafe-inline' 'unsafe-eval' https://cdn.plot.ly; \
                 style-src 'unsafe-inline'; img-src data: blob:; font-src data:; \
                 worker-src blob:; connect-src 'none'; base-uri 'none'; form-action 'none'"
            }
        }
    }
}

/// Content types a page would run as code from `<script src>` or
/// `WebAssembly.instantiateStreaming`. A render tree holds none of its
/// own, so one served from this origin came from upstream — and the app
/// page's `script-src 'self'` would let it run.
pub fn is_executable(content_type: &str) -> bool {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    matches!(
        mime.as_str(),
        "text/javascript"
            | "application/javascript"
            | "application/x-javascript"
            | "application/ecmascript"
            | "text/ecmascript"
            | "application/wasm"
    )
}

/// Content types a browser would run script from if it navigated to
/// them. Everything else — JSON, images as `<img>`, text — is inert.
pub fn is_scriptable_document(content_type: &str) -> bool {
    let mime = content_type
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase();
    matches!(
        mime.as_str(),
        "text/html"
            | "application/xhtml+xml"
            | "image/svg+xml"
            | "text/xml"
            | "application/xml"
            | "application/xslt+xml"
    ) || mime.ends_with("+xml")
}

/// Files the token gate lets through unauthenticated: the DACTAL page
/// and its scripts. They are static and identical on every install,
/// and the page runs sandboxed (see `serve_ui`), so it needs no
/// session — and, being sandboxed, could not use one: an opaque origin
/// sends no cookie with a module load.
pub const PUBLIC_PREFIX: &str = "dactal/";

pub async fn serve_ui(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.is_empty() {
        return serve_index();
    }
    match UiAssets::get(path) {
        Some(content) => asset_response(path, content),
        // A public path names a file or nothing: the SPA fallback would
        // hand the app shell to a caller with no session.
        None if path.starts_with(PUBLIC_PREFIX) => StatusCode::NOT_FOUND.into_response(),
        // An API call that names no endpoint must not read as a success.
        None if path.starts_with("api/") => {
            (StatusCode::NOT_FOUND, format!("no endpoint /{path}")).into_response()
        }
        // SPA fallback — let the client router handle unknown routes.
        None => serve_index(),
    }
}

fn serve_index() -> Response {
    match UiAssets::get("index.html") {
        Some(c) => asset_response("index.html", c),
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

fn asset_response(path: &str, content: rust_embed::EmbeddedFile) -> Response {
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
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(APP_CSP),
        );
    }
    if path.starts_with(PUBLIC_PREFIX) {
        // The sandboxed page loads `main.js` as a module, and a module
        // load from an opaque origin is a CORS request.
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_ORIGIN,
            HeaderValue::from_static("*"),
        );
        if is_scriptable_document(mime.as_ref()) {
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
    fn scriptable_documents_are_the_ones_a_browser_would_run() {
        for ct in [
            "text/html",
            "text/html; charset=utf-8",
            "TEXT/HTML",
            "image/svg+xml",
            "application/xhtml+xml",
            "application/xml",
            "application/rss+xml",
        ] {
            assert!(is_scriptable_document(ct), "{ct}");
        }
        for ct in [
            "application/json",
            "text/plain",
            "image/png",
            "text/markdown; charset=utf-8",
            "application/octet-stream",
        ] {
            assert!(!is_scriptable_document(ct), "{ct}");
        }
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
        let csp = DocumentKind::Data.csp();
        assert_eq!(directive(csp, "sandbox"), Some("sandbox"), "{csp}");
        assert_eq!(directive(csp, "default-src"), Some("default-src 'none'"));
        assert!(directive(csp, "script-src").is_none());
        assert!(directive(csp, "connect-src").is_none());
        for d in csp.split(';') {
            assert!(!d.contains("http"), "no remote origin in {d:?}");
        }
    }

    /// A plot page runs its own script and Plotly, and still sends
    /// nothing anywhere.
    #[test]
    fn plot_documents_run_plotly_and_reach_nothing() {
        let csp = DocumentKind::Plot.csp();
        assert!(!csp.contains("allow-same-origin"), "{csp}");
        assert_eq!(directive(csp, "default-src"), Some("default-src 'none'"));
        assert_eq!(directive(csp, "connect-src"), Some("connect-src 'none'"));
        assert_eq!(
            directive(csp, "script-src"),
            Some("script-src 'unsafe-inline' 'unsafe-eval' https://cdn.plot.ly")
        );
    }

    #[test]
    fn document_kinds_parse_as_they_print() {
        use strum::VariantArray;
        for &k in DocumentKind::VARIANTS {
            let s: &'static str = k.into();
            assert_eq!(DocumentKind::parse(s), Some(k));
        }
        assert_eq!(DocumentKind::parse("plot"), Some(DocumentKind::Plot));
        assert_eq!(DocumentKind::parse("script"), None);
    }

    #[test]
    fn executable_types_are_scripts_and_wasm() {
        for ct in [
            "text/javascript",
            "application/javascript; charset=utf-8",
            "application/wasm",
        ] {
            assert!(is_executable(ct), "{ct}");
        }
        for ct in ["text/plain", "application/json", "text/css", "text/html"] {
            assert!(!is_executable(ct), "{ct}");
        }
    }
}
