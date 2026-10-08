//! `datalib-applet datalib_contacts` — the contacts app: the one writer of
//! `datalib_curated/datalib_contacts/`, serving what a chip needs to resolve a
//! handle, what its popover needs to create or link a contact, and the
//! photo a person put on one.

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    http::{header, HeaderMap, StatusCode},
    middleware,
    response::{IntoResponse, Json, Response},
    routing::{get, post},
    Router,
};
use datalib_contacts::{ContactKind, Store};
use datalib_handle::Handle;
use serde::Deserialize;
use serde_json::json;

const DATA_ROOT_ENV: &str = "DATALIB_DAG_DATA_ROOT";
const SEARCH_LIMIT: u32 = 20;
/// What the photo route reads of a body: the gateway's own cap. axum's
/// default (2 MB) sits under `PHOTO_MAX_BYTES`, so without this a photo
/// between the two is refused by the framework in plain text before the
/// store can say why in its own words.
const PHOTO_BODY_LIMIT: usize = 8 * 1024 * 1024;

pub fn serve(port: u16) -> Result<()> {
    let root = std::env::var_os(DATA_ROOT_ENV)
        .map(PathBuf::from)
        .with_context(|| {
            format!(
                "{DATA_ROOT_ENV} is not set. The gateway sets it to the data root; \
                 to run this by hand, set it yourself"
            )
        })?;
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("build tokio runtime")?;
    rt.block_on(async move {
        let path = datalib_contacts::store_path(&root);
        let store = Store::open(&path)
            .await
            .with_context(|| format!("open {}", path.display()))?;
        let store = Arc::new(store);
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, port));
        let listener = tokio::net::TcpListener::bind(addr)
            .await
            .with_context(|| format!("bind {addr}"))?;
        let bound = listener.local_addr().context("read the bound address")?;
        let gate = Arc::new(crate::gate::Gate::from_env(bound.port())?);
        let app = routes(store).layer(middleware::from_fn_with_state(
            gate,
            crate::unified_index::require_gateway,
        ));
        tracing::info!(address = %bound, "listening");
        crate::announce_port(bound.port());
        axum::serve(listener, app).await.context("serve")
    })
}

/// Every route, over an open store; `serve` puts the gateway's check in
/// front of them.
fn routes(store: Arc<Store>) -> Router {
    Router::new()
        .route("/resolve", post(resolve))
        .route("/search", get(search))
        .route("/contacts", post(create))
        .route("/contact/{contact_id}", get(contact))
        .route("/link", post(link))
        .route("/unlink", post(unlink))
        .route("/stopped_working", post(stopped_working))
        .route("/rename", post(rename))
        .route(
            "/photo/{contact_id}",
            get(photo)
                .put(put_photo)
                .delete(delete_photo)
                .layer(DefaultBodyLimit::max(PHOTO_BODY_LIMIT)),
        )
        .route("/health", get(|| async { Json(json!({"ok": true})) }))
        .with_state(store)
}

type AppState = State<Arc<Store>>;

struct ApiError(StatusCode, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

/// A refusal the store explains (a handle someone else holds, a bad
/// date) is the person's to fix, so it goes back as a 409 with the
/// store's words; anything else is ours.
fn refused(e: anyhow::Error) -> ApiError {
    let msg = format!("{e:#}");
    let theirs = [
        "already belongs",
        "needs a name",
        "is not a date",
        "no contact",
        "is not a photo",
        "the photo is",
    ]
    .iter()
    .any(|m| msg.contains(m));
    if theirs {
        ApiError(StatusCode::CONFLICT, msg)
    } else {
        tracing::error!(error = %msg, "contacts store");
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, msg)
    }
}

fn handle(s: &str) -> Result<Handle, ApiError> {
    Handle::parse(s)
        .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, format!("{s:?} is not a handle")))
}

#[derive(Deserialize)]
struct ResolveBody {
    handles: Vec<String>,
}

/// A spelling this build does not parse is left unresolved rather than
/// failing the whole document's chips.
async fn resolve(
    State(store): AppState,
    Json(body): Json<ResolveBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let handles: Vec<Handle> = body
        .handles
        .iter()
        .filter_map(|h| Handle::parse(h))
        .collect();
    let resolved = store.resolve(&handles).await.map_err(refused)?;
    Ok(Json(json!({ "resolved": resolved })))
}

#[derive(Deserialize)]
struct SearchQuery {
    #[serde(default)]
    q: String,
}

async fn search(
    State(store): AppState,
    Query(q): Query<SearchQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let contacts = store
        .search(q.q.trim(), SEARCH_LIMIT)
        .await
        .map_err(refused)?;
    Ok(Json(json!({ "contacts": contacts })))
}

#[derive(Deserialize)]
struct CreateBody {
    name: String,
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    handles: Vec<String>,
}

async fn create(
    State(store): AppState,
    Json(body): Json<CreateBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let kind = match body.kind.as_deref() {
        None => ContactKind::Person,
        Some(k) => ContactKind::parse(k).ok_or_else(|| {
            ApiError(
                StatusCode::BAD_REQUEST,
                format!("{k:?} is not a contact kind"),
            )
        })?,
    };
    let handles = body
        .handles
        .iter()
        .map(|h| handle(h))
        .collect::<Result<Vec<_>, _>>()?;
    let contact_id = store
        .create(&body.name, kind, &handles)
        .await
        .map_err(refused)?;
    Ok(Json(json!({ "contact_id": contact_id })))
}

async fn contact(
    State(store): AppState,
    Path(contact_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    match store.contact(&contact_id).await.map_err(refused)? {
        Some(c) => Ok(Json(json!(c))),
        None => Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no contact {contact_id}"),
        )),
    }
}

#[derive(Deserialize)]
struct LinkBody {
    handle: String,
    contact_id: String,
}

async fn link(
    State(store): AppState,
    Json(body): Json<LinkBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    store
        .link(&handle(&body.handle)?, &body.contact_id)
        .await
        .map_err(refused)?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct HandleBody {
    handle: String,
}

async fn unlink(
    State(store): AppState,
    Json(body): Json<HandleBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let was_linked = store
        .unlink(&handle(&body.handle)?)
        .await
        .map_err(refused)?;
    Ok(Json(json!({ "was_linked": was_linked })))
}

#[derive(Deserialize)]
struct StoppedBody {
    handle: String,
    by: Option<String>,
}

async fn stopped_working(
    State(store): AppState,
    Json(body): Json<StoppedBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let found = store
        .set_stopped_working(&handle(&body.handle)?, body.by.as_deref())
        .await
        .map_err(refused)?;
    Ok(Json(json!({ "found": found })))
}

/// The photo itself, with its type, for an `<img>`.
async fn photo(
    State(store): AppState,
    Path(contact_id): Path<String>,
) -> Result<Response, ApiError> {
    match store.photo(&contact_id).await.map_err(refused)? {
        Some((content_type, bytes)) => Ok((
            StatusCode::OK,
            [(header::CONTENT_TYPE, content_type)],
            bytes,
        )
            .into_response()),
        None => Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no photo for {contact_id}"),
        )),
    }
}

/// The body is the image; its `Content-Type` says what kind.
async fn put_photo(
    State(store): AppState,
    Path(contact_id): Path<String>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<serde_json::Value>, ApiError> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");
    store
        .set_photo(&contact_id, content_type, &body)
        .await
        .map_err(refused)?;
    Ok(Json(
        json!({ "photo_url": datalib_contacts::photo_url(&contact_id) }),
    ))
}

async fn delete_photo(
    State(store): AppState,
    Path(contact_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let had = store.clear_photo(&contact_id).await.map_err(refused)?;
    Ok(Json(json!({ "had_photo": had })))
}

#[derive(Deserialize)]
struct RenameBody {
    contact_id: String,
    name: String,
}

async fn rename(
    State(store): AppState,
    Json(body): Json<RenameBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let found = store
        .rename(&body.contact_id, &body.name)
        .await
        .map_err(refused)?;
    Ok(Json(json!({ "found": found })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    async fn put(app: &Router, contact_id: &str, len: usize) -> (StatusCode, String) {
        let resp = app
            .clone()
            .oneshot(
                Request::put(format!("/photo/{contact_id}"))
                    .header(header::CONTENT_TYPE, "image/png")
                    .body(Body::from(vec![0u8; len]))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    /// A photo up to the store's limit is taken, and one over it is
    /// refused in the store's words. axum's own 2 MB default once
    /// answered both a plain-text 413 before the store saw them.
    #[tokio::test]
    async fn a_photo_is_judged_by_the_stores_limit_not_the_frameworks() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&datalib_contacts::store_path(dir.path()))
            .await
            .unwrap();
        let id = store
            .create("Will Riker", ContactKind::Person, &[])
            .await
            .unwrap();
        let store = Arc::new(store);
        let app = routes(store.clone());

        let (status, body) = put(&app, &id, 3 * 1024 * 1024).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = put(&app, &id, 5 * 1024 * 1024).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body.contains("the most a contact's photo can be"), "{body}");

        drop(app);
        Arc::try_unwrap(store)
            .ok()
            .expect("the router is gone")
            .close()
            .await;
    }
}
