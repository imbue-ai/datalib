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
//! stores it, the watch is told it for the frame the line causes, and a
//! chain long enough to be a loop is warned about here (`loop_guard`).
//!
//! These are requests the app's own pages make of this server, not ones
//! that leave the machine (`http.outbound` is those). A page reading what
//! it draws is a `debug` line, whatever made it ask: only what changes
//! something, or what went wrong, is in the card's default view
//! ([`worth`]).
//!
//! The line says how many rows the request's statements returned beside
//! the size of the answer, so one that reads far more than it answers with
//! shows on its own line.

use std::time::Instant;

use axum::body::{Body, HttpBody};
use axum::extract::{Request, State};
use axum::http::{Method, StatusCode};
use axum::middleware::Next;
use axum::response::Response;

/// The tracing target every request line carries; `target:http.request`
/// in the log panel's search bar is the request log.
pub const TARGET: &str = "http.request";

/// POSTs that change nothing a person asked to change: a page asking what
/// to draw, or reporting what it did.
const ROUTINE_POSTS: [&str; 9] = [
    "/api/config/check",
    "/api/entities",
    "/api/manage/rows/refresh",
    "/api/pipeline/storage/refresh",
    "/api/remote_media/check",
    "/api/ui/events",
    "/applet/unified_grid_and_search_index/people",
    "/applet/unified_grid_and_search_index/index_state",
    "/applet/unified_grid_and_search_index/sql",
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

/// On a proxied response: the rows the applet says it read and what its
/// statements did, if it says.
#[derive(Debug, Clone)]
pub struct AppletWork {
    pub rows_read: Option<u64>,
    pub sql_work: Option<String>,
}

pub async fn record(
    State(echoes): State<crate::loop_guard::Echoes>,
    req: Request<Body>,
    next: Next,
) -> Response {
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
    let (resp, own) =
        datalib_sql_work::counted(crate::loop_guard::scope(chain, next.run(req))).await;
    let status = resp.status();
    if !is_logged(&path, status) {
        return resp;
    }
    echoes.logged(chain);
    let ms = started.elapsed().as_millis() as u64;
    // Not the `Content-Length` header: axum sets that outside this layer.
    let bytes = resp.body().size_hint().exact();
    let (rows_read, sql, sql_ms) = match resp.extensions().get::<AppletWork>() {
        Some(theirs) => {
            let statements = theirs
                .sql_work
                .as_deref()
                .and_then(|h| serde_json::from_str(h).ok());
            (
                theirs.rows_read,
                theirs.sql_work.clone(),
                statements.as_ref().map(statements_ms),
            )
        }
        None => (
            own.as_ref().map(|c| c.rows),
            own.as_ref()
                .filter(|c| !c.statements.is_empty())
                .map(|c| datalib_sql_work::header_value(&c.statements)),
            own.as_ref().map(|c| statements_ms(&c.statements)),
        ),
    };
    let worth = worth(&method, &path, status, live_refetch);
    let sql = sql.filter(|_| shows_statements(worth, ms));
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
    let cost = cost(ms, rows_read, bytes);
    macro_rules! write_line {
        ($level:ident) => {
            tracing::$level!(
                target: TARGET,
                method = %method, path = %path, query = query.as_deref(), status = code,
                ms, rows_read, bytes,
                page = page.as_deref(), card = card.as_deref(), card_type = card_type.as_deref(),
                chain, sql_ms, sql = sql.as_deref(),
                "{method} {path} {code} {cost}"
            )
        };
    }
    match worth {
        Worth::Warn => write_line!(warn),
        Worth::Debug => write_line!(debug),
        Worth::Info => write_line!(info),
    }
    resp
}

/// A request that took this long is worth its statements on its line,
/// whatever it was.
const SLOW_REQUEST_MS: u64 = 1_000;

/// Whether a line carries its statements one by one. A page polls the
/// same few routes about once a second, and their statements, some 3 KB
/// a line, were most of the log's bytes; every line keeps `sql_ms`, and
/// `GET /metrics` has each statement's totals.
fn shows_statements(worth: Worth, ms: u64) -> bool {
    worth != Worth::Debug || ms >= SLOW_REQUEST_MS
}

/// The time the request's statements took, in all.
fn statements_ms(statements: &datalib_sql_work::ByTag) -> u64 {
    statements.values().map(|w| w.micros).sum::<u64>() / 1_000
}

/// What a request took and what it answered with, for a person reading
/// the line. A part that is not known is left out.
fn cost(ms: u64, rows_read: Option<u64>, bytes: Option<u64>) -> String {
    let mut parts = vec![format!("{ms}ms")];
    parts.extend(rows_read.map(|n| format!("{n} rows read")));
    parts.extend(bytes.map(|n| format!("{n} bytes")));
    parts.join(", ")
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

    /// Guards the log's size: the Manage page's two polls, each about once
    /// a second, carried 3.7 KB of statements a line.
    #[test]
    fn only_a_line_worth_reading_or_a_slow_one_names_its_statements() {
        assert!(!shows_statements(Worth::Debug, 150));
        assert!(shows_statements(Worth::Debug, SLOW_REQUEST_MS));
        assert!(shows_statements(Worth::Info, 3));
        assert!(shows_statements(Worth::Warn, 3));
    }

    #[test]
    fn a_line_says_what_is_known_of_its_cost() {
        assert_eq!(
            cost(4500, Some(31_303), Some(4_821)),
            "4500ms, 31303 rows read, 4821 bytes"
        );
        assert_eq!(cost(3, Some(0), None), "3ms, 0 rows read");
        assert_eq!(cost(3, None, None), "3ms");
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
            "/applet/unified_grid_and_search_index/rows",
            StatusCode::OK
        ));
    }
}
