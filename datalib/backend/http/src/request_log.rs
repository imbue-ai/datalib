//! One log line per request (`docs/dev/logging.md`). Outermost on the
//! router, so a refused request is a line too; written when the
//! response *headers* are ready, so for a stream it is the open.
//!
//! Two kinds of request write nothing. A read of the log itself: the
//! log card refetches whenever the log moves, so a line per read would
//! wake it into a loop against its own store. And the bundle's
//! content-hashed assets and the component modules, unless they failed:
//! a page load is dozens of them and the browser caches them forever.
//!
//! A request a card made names the card and its type, so the log can
//! say which card is noisy.
//!
//! A request a `root` frame caused carries that frame's chain; the line
//! stores it, and a chain long enough to be a loop is warned about here
//! (`loop_guard`).
//!
//! These are requests the app's own pages make of this server, not ones
//! that leave the machine (`http.outbound` is those). A page reading what
//! it draws is a `debug` line, whatever made it ask: only what changes
//! something, or what went wrong, is in the card's default view
//! ([`worth`]).

use std::time::Instant;

use axum::body::Body;
use axum::extract::Request;
use axum::http::{header, Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

/// The tracing target every request line carries; `target:http.request`
/// in the log panel's search bar is the request log.
pub const TARGET: &str = "http.request";

/// POSTs that change nothing a person asked to change: a page asking what
/// to draw, or reporting what it did.
const ROUTINE_POSTS: [&str; 8] = [
    "/api/config/check",
    "/api/entities",
    "/api/manage/rows/refresh",
    "/api/pipeline/storage/refresh",
    "/api/remote_media/check",
    "/api/ui/events",
    "/applet/unified_document_index/people",
    "/applet/unified_document_index/qmd_state",
];

/// Where a page keeps its own layout: written on every drag.
const UI_STATE: &str = "/api/ui/state/";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Worth {
    Debug,
    Info,
    Warn,
}

/// The level of a request's line, which is the one thing that decides
/// whether a person reading the log at `info` sees it.
fn worth(method: &Method, path: &str, status: StatusCode, live_refetch: bool) -> Worth {
    if status.is_server_error() {
        return Worth::Warn;
    }
    if !(status.is_success() || status.is_redirection()) {
        return Worth::Info;
    }
    let reads = matches!(*method, Method::GET | Method::HEAD);
    let routine = (*method == Method::POST && ROUTINE_POSTS.contains(&path))
        || (*method == Method::PUT && path.starts_with(UI_STATE));
    if reads || routine || live_refetch {
        Worth::Debug
    } else {
        Worth::Info
    }
}

pub async fn record(req: Request<Body>, next: Next) -> Response {
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let query = req.uri().query().and_then(crate::auth::query_without_token);
    let page = header_text(&req, crate::ui_events::PAGE_HEADER);
    let card = header_text(&req, crate::ui_events::CARD_HEADER);
    let card_type = header_text(&req, crate::ui_events::CARD_TYPE_HEADER);
    let cause = req
        .headers()
        .get(crate::loop_guard::CAUSE_HEADER)
        .and_then(|v| v.to_str().ok());
    let live_refetch = cause.is_some();
    let chain = crate::loop_guard::request_chain(cause);
    let started = Instant::now();
    let resp = crate::loop_guard::scope(chain, next.run(req)).await;
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
    // Zero is every request nothing caused; leaving it off keeps the
    // line as it was for them.
    let chain = (chain > 0).then_some(chain);
    if let Some(c) = chain.filter(|&c| crate::loop_guard::warns_at(c)) {
        tracing::warn!(
            target: crate::loop_guard::TARGET,
            method = %method, path = %path, query = query.as_deref(), chain = c,
            page = page.as_deref(), card = card.as_deref(), card_type = card_type.as_deref(),
            "{method} {path} is refetching on its own echo: {c} requests in a row, \
             each caused by the frame the one before it caused"
        );
    }
    match worth(&method, &path, status, live_refetch) {
        Worth::Warn => tracing::warn!(
            target: TARGET,
            method = %method, path = %path, query = query.as_deref(), status = code, ms, bytes,
            page = page.as_deref(), card = card.as_deref(), card_type = card_type.as_deref(), chain,
            "{method} {path} {code} {ms}ms"
        ),
        Worth::Debug => tracing::debug!(
            target: TARGET,
            method = %method, path = %path, query = query.as_deref(), status = code, ms, bytes,
            page = page.as_deref(), card = card.as_deref(), card_type = card_type.as_deref(), chain,
            "{method} {path} {code} {ms}ms"
        ),
        Worth::Info => tracing::info!(
            target: TARGET,
            method = %method, path = %path, query = query.as_deref(), status = code, ms, bytes,
            page = page.as_deref(), card = card.as_deref(), card_type = card_type.as_deref(), chain,
            "{method} {path} {code} {ms}ms"
        ),
    }
    resp
}

fn header_text(req: &Request<Body>, name: &str) -> Option<String> {
    req.headers()
        .get(name)
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
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

    /// Guards the log that misled a reader: `POST /api/entities`, three
    /// hundred lines in nine minutes, read as traffic in the default view,
    /// and `GET /api/config` flipped level with what triggered it.
    #[test]
    fn only_a_change_or_a_failure_is_above_debug() {
        let ok = StatusCode::OK;
        let at = |m: Method, path: &str, status, live| worth(&m, path, status, live);
        assert_eq!(at(Method::GET, "/api/config", ok, false), Worth::Debug);
        assert_eq!(at(Method::GET, "/api/config", ok, true), Worth::Debug);
        assert_eq!(at(Method::POST, "/api/entities", ok, false), Worth::Debug);
        assert_eq!(
            at(Method::PUT, "/api/ui/state/layout", ok, false),
            Worth::Debug
        );

        assert_eq!(at(Method::POST, "/api/requests", ok, false), Worth::Info);
        assert_eq!(at(Method::POST, "/api/reset", ok, false), Worth::Info);
        assert_eq!(
            at(Method::PUT, "/api/config", StatusCode::NO_CONTENT, false),
            Worth::Info
        );
        assert_eq!(
            at(Method::POST, "/applet/datalib_contacts/link", ok, false),
            Worth::Info
        );

        let refused = StatusCode::UNAUTHORIZED;
        assert_eq!(at(Method::GET, "/api/config", refused, true), Worth::Info);
        let broke = StatusCode::INTERNAL_SERVER_ERROR;
        assert_eq!(at(Method::GET, "/api/config", broke, true), Worth::Warn);
    }

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
        assert!(is_logged(
            "/applet/unified_document_index/rows",
            StatusCode::OK
        ));
    }
}
