//! `GET /data/<group>/<function>/<kind>/…`: what a step keeps, by the
//! address a page links it with. The route knows the blob store's shape
//! (`datalib/backend/blob_store/README.md`) and nothing else about a
//! step; it picks a handler by the kind the path names.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use strum::EnumString;

use crate::embed::{is_executable, is_scriptable_document, DATA_DOCUMENT_CSP};
use crate::AppState;

/// What an address under a step names. One kind so far; a kind this
/// build does not know is "not found", like any address it cannot answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString)]
#[strum(serialize_all = "snake_case")]
enum DataKind {
    /// A row of the step's blob store, by its hash.
    Blob,
}

pub async fn get_data(
    State(s): State<AppState>,
    Path((group, function, kind, rest)): Path<(String, String, String, String)>,
) -> Response<Body> {
    if !is_a_groups_tree(&s, &group, &function) {
        return not_found();
    }
    match kind.parse::<DataKind>() {
        Ok(DataKind::Blob) => blob(&s, &group, &function, &rest).await,
        Err(_) => not_found(),
    }
}

/// Whether `<group>/<function>` is a tree under one of the config's
/// groups. The group is looked up, never taken as given, so the path
/// cannot reach `system/` or anything else that is not a group's. The
/// step itself need not be declared: a source can be render-only, its
/// render reading an `ingest` tree no step of this config writes.
fn is_a_groups_tree(s: &AppState, group: &str, function: &str) -> bool {
    let plain = |seg: &str| !seg.is_empty() && !seg.starts_with('.');
    plain(function)
        && crate::supervisor::load_config(&s.root)
            .is_ok_and(|c| c.cfg.groups.iter().any(|g| g.id == group))
}

async fn blob(s: &AppState, group: &str, function: &str, hash: &str) -> Response<Body> {
    if !datalib_blob_store::is_hash(hash) {
        return not_found();
    }
    let path = s
        .root
        .join(group)
        .join(function)
        .join(datalib_blob_store::BLOBS_DB);
    match datalib_blob_store::read(&path, hash).await {
        Ok(Some(stored)) => {
            let content_type = stored
                .content_type
                .unwrap_or_else(|| "application/octet-stream".to_string());
            response(&content_type, stored.bytes)
        }
        Ok(None) => not_found(),
        Err(e) => {
            tracing::error!(store = %path.display(), error = %e, "read a blob");
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                "could not read the blob store",
            )
                .into_response()
        }
    }
}

/// The bytes, kept by the browser for good since an address never names
/// other bytes. A document a sender wrote runs nothing in our origin, and
/// script goes out as plain bytes, as from the applet gateway.
fn response(content_type: &str, body: Vec<u8>) -> Response<Body> {
    let served = if is_executable(content_type) {
        "application/octet-stream"
    } else {
        content_type
    };
    let mut resp = Response::new(Body::from(body));
    let headers = resp.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(served)
            .unwrap_or(HeaderValue::from_static("application/octet-stream")),
    );
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    headers.insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    if is_scriptable_document(content_type) {
        headers.insert(
            header::CONTENT_SECURITY_POLICY,
            HeaderValue::from_static(DATA_DOCUMENT_CSP),
        );
    }
    resp
}

fn not_found() -> Response<Body> {
    StatusCode::NOT_FOUND.into_response()
}
