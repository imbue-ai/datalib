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
/// - `img-src`/`media-src` any `http(s):`: a rendered email or chat may
///   reference a remote image, as it did before the policy existed.
/// - `connect-src ipc: http://ipc.localhost`: Tauri's IPC. The desktop
///   shell loads this page from the server as a remote URL, so Tauri
///   does not rewrite the policy the way it would for a page it serves
///   itself, and its `invoke` is a `fetch` to those origins.
/// - `frame-src 'self'`: the DACTAL card and the plot pages are
///   same-origin iframes — which is why every document the applet
///   proxy serves gets [`DOCUMENT_SANDBOX_CSP`].
pub const APP_CSP: &str = "default-src 'self'; \
    script-src 'self' 'unsafe-eval'; \
    style-src 'self' 'unsafe-inline'; \
    img-src 'self' data: blob: http: https:; \
    media-src 'self' data: blob: http: https:; \
    font-src 'self' data:; \
    connect-src 'self' ipc: http://ipc.localhost; \
    frame-src 'self'; \
    worker-src 'self' blob:; \
    object-src 'none'; \
    base-uri 'none'; \
    form-action 'self'; \
    frame-ancestors 'none'";

/// The policy on every document that is *data*, not the app: a rendered
/// plot page, an attachment, anything under `/dactal/`. `sandbox` puts
/// the document in an opaque origin — its scripts may run (a plot page
/// needs its own), but it has no cookie, no same-origin `fetch` to
/// `/api/*`, and no way to navigate the window it sits in. Sent as a
/// header because `sandbox` is ignored in a `<meta>` policy.
pub const DOCUMENT_SANDBOX_CSP: &str = "sandbox allow-scripts allow-downloads";

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
                HeaderValue::from_static(DOCUMENT_SANDBOX_CSP),
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
}
