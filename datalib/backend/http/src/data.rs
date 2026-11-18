//! `GET /data/<group>/<function>/<kind>/…`: what a step keeps, by the
//! address a page links it with. The route knows the blob store's shape
//! (`datalib/backend/blob_store/README.md`) and nothing else about a
//! step; it picks a handler by the kind the path names.

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{header, HeaderValue, Response, StatusCode};
use axum::response::IntoResponse;
use strum::EnumString;

use crate::embed::set_data_headers;
use crate::AppState;

/// What an address under a step names. One kind so far; a kind this
/// build does not know is "not found", like any address it cannot answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, EnumString)]
#[strum(serialize_all = "snake_case")]
enum DataKind {
    /// A row of the step's blob store, by its hash; or a MIME part of a
    /// message stored there, `<hash>/part/<hash of the part's bytes>`.
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
/// groups. The group is looked up, never taken as given, and the function
/// must be one folder name (the router decodes `%2F`, so it can arrive
/// holding a `/`), so the path cannot reach `system/` or anything else
/// that is not a group's. The step itself need not be declared: a source
/// can be render-only, its render reading an `ingest` tree no step of
/// this config writes.
fn is_a_groups_tree(s: &AppState, group: &str, function: &str) -> bool {
    datalib_dag::config::usable_function(function)
        && crate::supervisor::load_config(&s.root)
            .is_ok_and(|c| c.cfg.groups.iter().any(|g| g.id == group))
}

async fn blob(s: &AppState, group: &str, function: &str, rest: &str) -> Response<Body> {
    let (hash, part) = match rest.split('/').collect::<Vec<_>>()[..] {
        [hash] => (hash, None),
        [hash, "part", part] => (hash, Some(part)),
        _ => return not_found(),
    };
    if !datalib_blob_store::is_hash(hash) || part.is_some_and(|p| !datalib_blob_store::is_hash(p)) {
        return not_found();
    }
    let path = s
        .root
        .join(group)
        .join(function)
        .join(datalib_blob_store::BLOBS_DB);
    let read = match part {
        None => datalib_blob_store::read(&path, hash).await,
        Some(part) => datalib_blob_store::read_part(&path, hash, part).await,
    };
    match read {
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
/// other bytes, and served as data: they run nothing in our origin.
fn response(content_type: &str, body: Vec<u8>) -> Response<Body> {
    let mut resp = Response::new(Body::from(body));
    let headers = resp.headers_mut();
    set_data_headers(headers, content_type);
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=31536000, immutable"),
    );
    resp
}

fn not_found() -> Response<Body> {
    StatusCode::NOT_FOUND.into_response()
}
