//! Embeds the Vite-built UI into the binary via `rust-embed`, then
//! serves it through axum.
//!
//! Folder path is `$FRANKWEILER_UI_DIST`, set at proc-macro time by:
//!   - Bazel (`rust_library.rustc_env`) — points at the bazel-out
//!     directory produced by `//frankweiler/ui:dist`.
//!   - Cargo — caller must export it explicitly (cargo isn't used to
//!     build the http crate today because the workspace's sqlx-sqlite
//!     `unbundled` feature wants Bazel-built doltlite headers; if that
//!     changes, add a `build.rs` that sets a sensible default).
//!
//! The rust-embed `interpolate-folder-path` feature does the env-var
//! substitution; `debug-embed` ensures bytes are baked into the binary
//! even in debug builds (otherwise debug mode reads files from the
//! compile-time path at runtime, which fails outside the sandbox).
//!
//! SPA fallback: any GET that doesn't match a static asset returns
//! `index.html` (200), so client-side routing works. API routes are
//! matched first in the router, so this only runs for genuinely
//! unmatched paths.

use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode, Uri};
use axum::response::{IntoResponse, Response};
use rust_embed::RustEmbed;

#[derive(RustEmbed)]
#[folder = "$FRANKWEILER_UI_DIST"]
struct UiAssets;

pub async fn serve_ui(uri: Uri) -> Response {
    let path = uri.path().trim_start_matches('/');
    if path.is_empty() {
        return serve_index();
    }
    match UiAssets::get(path) {
        Some(content) => asset_response(path, content),
        // SPA fallback — let the client router handle unknown routes.
        None => serve_index(),
    }
}

fn serve_index() -> Response {
    match UiAssets::get("index.html") {
        Some(c) => asset_response("index.html", c),
        // Built without a UI bundle present. Surface a clear error
        // rather than a confusing 404 — the right fix is to populate
        // FRANKWEILER_UI_DIST and rebuild.
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
    resp
}
