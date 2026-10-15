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
//! What a person typed stays out of the line (`docs/dev/logging.md`
//! § "What a line may carry"): a query string keeps its keys, and its
//! values only for the keys in [`KEPT_VALUES`]; an app route keeps the
//! names of the cards it opens and not their arguments, where a grid's
//! search lives.
//!
//! A request a `root` frame caused carries that frame's chain; the line
//! stores it, and a chain long enough to be a loop is warned about here
//! (`loop_guard`). Such a request is a live refetch: while a sync runs
//! the Manage rows alone are one a second, so one that succeeded is a
//! `debug` line, kept for the loop guard and out of the card's default
//! `min_level:info` view.

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
    let path = path_for_log(req.uri().path());
    let query = req.uri().query().and_then(query_for_log);
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
    if status.is_server_error() {
        tracing::warn!(
            target: TARGET,
            method = %method, path = %path, query = query.as_deref(), status = code, ms, bytes,
            page = page.as_deref(), card = card.as_deref(), card_type = card_type.as_deref(), chain,
            "{method} {path} {code} {ms}ms"
        );
    } else if live_refetch && status.is_success() {
        tracing::debug!(
            target: TARGET,
            method = %method, path = %path, query = query.as_deref(), status = code, ms, bytes,
            page = page.as_deref(), card = card.as_deref(), card_type = card_type.as_deref(), chain,
            "{method} {path} {code} {ms}ms"
        );
    } else {
        tracing::info!(
            target: TARGET,
            method = %method, path = %path, query = query.as_deref(), status = code, ms, bytes,
            page = page.as_deref(), card = card.as_deref(), card_type = card_type.as_deref(), chain,
            "{method} {path} {code} {ms}ms"
        );
    }
    resp
}

/// The query-string keys whose values we mint or name ourselves — a
/// count, a cursor, a run, step or row id, a column, a tab — so they
/// carry nothing a person typed or a record held. A key not listed
/// keeps its name and loses its value, so a new parameter is private
/// until someone adds it here.
const KEPT_VALUES: &[&str] = &[
    "after_seq",
    "attempt",
    "before_seq",
    "by",
    "key",
    "limit",
    "offset",
    "process",
    "refresh",
    "run",
    "sort",
    "step",
    "tab",
    "through",
    "tree",
];

const REDACTED: &str = "<redacted>";

fn query_for_log(query: &str) -> Option<String> {
    let pairs: Vec<String> = query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            if key == crate::auth::TOKEN_QUERY_KEY {
                None
            } else if value.is_empty() || KEPT_VALUES.contains(&key) {
                Some(pair.to_string())
            } else {
                Some(format!("{key}={REDACTED}"))
            }
        })
        .collect();
    (!pairs.is_empty()).then(|| pairs.join("&"))
}

/// The paths the server routes itself. Anything else is the app's own
/// route, whose segments are the cards on screen, written as code
/// (`gridView({q:"…"})`, `router/columns.ts`); a new server prefix
/// missing here costs that route's detail in the log, nothing more.
fn is_server_path(path: &str) -> bool {
    [
        "/api/",
        "/applet/",
        "/modules/",
        "/assets/",
        "/agent",
        "/metrics",
    ]
    .iter()
    .any(|p| path.starts_with(p))
        || path
            .strip_prefix('/')
            .is_some_and(|p| p.starts_with(crate::embed::PUBLIC_PREFIX))
}

fn path_for_log(path: &str) -> String {
    if is_server_path(path) {
        return path.to_string();
    }
    let card_name = |segment: &str| -> String {
        segment
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
            .collect()
    };
    let names: Vec<String> = path.split('/').map(card_name).collect();
    names.join("/")
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

    #[test]
    fn the_logs_own_reads_are_not_logged() {
        assert!(!is_logged("/api/log", StatusCode::OK));
        assert!(!is_logged("/api/runs/r-1/log", StatusCode::OK));
        assert!(is_logged("/api/runs/r-1/steps", StatusCode::OK));
        assert!(is_logged("/api/runs", StatusCode::OK));
    }

    #[test]
    fn a_query_keeps_its_keys_and_only_the_values_we_mint() {
        assert_eq!(query_for_log("token=abc"), None);
        assert_eq!(
            query_for_log("q=alice%20smith&limit=50&token=abc&sort=created_at:desc"),
            Some("q=<redacted>&limit=50&sort=created_at:desc".into())
        );
        assert_eq!(
            query_for_log("key=author&typed=al&q=&url=https%3A%2F%2Fx"),
            Some("key=author&typed=<redacted>&q=&url=<redacted>".into())
        );
        assert_eq!(
            query_for_log("refresh&x=1"),
            Some("refresh&x=<redacted>".into())
        );
    }

    #[test]
    fn an_app_route_keeps_its_card_names_and_drops_their_arguments() {
        assert_eq!(
            path_for_log("/gridView(%7Bq%3A%22alice%22%7D)::abc/logView()"),
            "/gridView/logView"
        );
        assert_eq!(path_for_log("/comp.user.tetris:1.5"), "/comp.user.tetris");
        assert_eq!(path_for_log("/"), "/");
        assert_eq!(path_for_log("/favicon.svg"), "/favicon.svg");
        assert_eq!(path_for_log("/api/runs/r-1/steps"), "/api/runs/r-1/steps");
        assert_eq!(
            path_for_log("/applet/unified_index/search"),
            "/applet/unified_index/search"
        );
        assert_eq!(path_for_log("/dactal/main.js"), "/dactal/main.js");
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
