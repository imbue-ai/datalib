//! Embeds the Vite-built UI (`frankweiler/ui/dist/`) into the binary
//! via `rust-embed`, then serves it through axum.
//!
//! Path is relative to this crate's `CARGO_MANIFEST_DIR` — set
//! identically by cargo and by rules_rust, so the same literal works
//! in both. The `compile_data = [//frankweiler/ui:dist_files]` line in
//! BUILD.bazel makes the files visible under the same relative path
//! inside the Bazel execroot. A follow-up will introduce a
//! `vite_build` rule whose output replaces the source-tree dist; at
//! that point this becomes `$FRANKWEILER_UI_DIST` interpolation with
//! the `interpolate-folder-path` rust-embed feature enabled.
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
#[folder = "../../ui/dist"]
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
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(mime.as_ref()).unwrap_or(HeaderValue::from_static("text/plain")),
    );
    resp
}
