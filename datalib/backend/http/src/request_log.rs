//! One log line per request, so the server's log says what the app
//! asked for, when, and how long it took — the record of what a person
//! did at the screen, until the UI reports its own actions.
//!
//! Outermost on the router, so a refused request is a line too. The
//! line is written when the response *headers* are ready: for a JSON
//! handler that is the whole request; for the SSE stream and an applet
//! proxy whose body streams, it is the open.
//!
//! Two kinds of request write nothing. A read of the log itself: the
//! log panel refetches whenever the log moves, so a line per read would
//! wake it into a loop against its own store. And the bundle's
//! content-hashed assets and the component modules, unless they failed:
//! a page load is dozens of them and the browser caches them forever.

use std::time::Instant;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

/// The tracing target every request line carries; `target:http.request`
/// in the log panel's search bar is the request log.
pub const TARGET: &str = "http.request";

pub async fn record(req: Request<Body>, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().and_then(crate::auth::query_without_token);
    let started = Instant::now();
    let resp = next.run(req).await;
    let status = resp.status();
    if !is_logged(&path, status) {
        return resp;
    }
    let ms = started.elapsed().as_millis() as u64;
    let bytes = resp
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let code = status.as_u16();
    if status.is_server_error() {
        tracing::warn!(
            target: TARGET,
            method = %method, path = %path, query = query.as_deref(), status = code, ms, bytes,
            "{method} {path} {code} {ms}ms"
        );
    } else {
        tracing::info!(
            target: TARGET,
            method = %method, path = %path, query = query.as_deref(), status = code, ms, bytes,
            "{method} {path} {code} {ms}ms"
        );
    }
    resp
}

fn is_logged(path: &str, status: StatusCode) -> bool {
    if reads_the_log(path) {
        return false;
    }
    if is_cached_asset(path) {
        return status.is_client_error() || status.is_server_error();
    }
    true
}

/// `GET /api/log` and `GET /api/runs/{run}/log`: what the log panel
/// fetches on every `log` frame.
fn reads_the_log(path: &str) -> bool {
    path == "/api/log" || (path.starts_with("/api/runs/") && path.ends_with("/log"))
}

fn is_cached_asset(path: &str) -> bool {
    path.starts_with("/assets/")
        || path.starts_with("/modules/")
        || path
            .strip_prefix('/')
            .is_some_and(|p| p.starts_with(crate::embed::PUBLIC_PREFIX))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_logs_own_reads_are_not_logged() {
        assert!(!is_logged("/api/log", StatusCode::OK));
        assert!(!is_logged("/api/runs/r-1/log", StatusCode::OK));
        assert!(is_logged("/api/runs/r-1/steps", StatusCode::OK));
        assert!(is_logged("/api/runs", StatusCode::OK));
    }

    #[test]
    fn cached_assets_are_logged_only_when_they_fail() {
        assert!(!is_logged("/assets/index-abc123.js", StatusCode::OK));
        assert!(!is_logged("/modules/deadbeef", StatusCode::OK));
        assert!(!is_logged("/dactal/main.js", StatusCode::OK));
        assert!(is_logged("/modules/deadbeef", StatusCode::NOT_FOUND));
        assert!(is_logged("/dactal/nope.js", StatusCode::NOT_FOUND));
        assert!(is_logged("/", StatusCode::OK));
        assert!(is_logged("/api/dag", StatusCode::OK));
        assert!(is_logged("/applet/unified_index/rows", StatusCode::OK));
    }
}
