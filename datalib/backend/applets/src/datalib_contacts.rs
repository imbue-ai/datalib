//! `datalib-applet datalib_contacts` — the contacts app: the one writer of
//! `datalib_curated/datalib_contacts/`, serving what a chip needs to resolve a
//! handle, what its popover needs to create or link a contact, the
//! photo a person put on one, the draft a contact is edited through
//! on its card, the table of every contact the app's Browse opens, and
//! the review of what agents proposed (`review.rs`). `GET /` lists every
//! route (`api.rs`).

mod api;
mod review;

use std::net::{Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context, Result};
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Path, Query, State},
    handler::Handler,
    http::{header, HeaderMap, StatusCode},
    middleware,
    response::{IntoResponse, Json, Response},
    Router,
};
use datalib_columns::{Chip, ChipKind, ColumnSpec, ColumnType, Identity};
use datalib_contact_schema::{Medium, NormalizedContact};
use datalib_contacts::drafts::ContactEdit;
use datalib_contacts::{ContactKind, Store};
use datalib_handle::Handle;
use serde::Deserialize;
use serde_json::json;

use api::{Api, RouteKind, Verb};

const DATA_ROOT_ENV: &str = datalib_env::DATALIB_DAG_DATA_ROOT;
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
            crate::unified_document_index::require_gateway,
        ));
        tracing::info!(address = %bound, "listening");
        crate::announce_port(bound.port());
        axum::serve(listener, app).await.context("serve")
    })
}

/// Every route, over an open store; `serve` puts the gateway's check in
/// front of them.
fn routes(store: Arc<Store>) -> Router {
    api().into_router().with_state(store)
}

/// What `datalib-applet datalib_contacts --help` prints after its flags.
pub fn routes_help() -> String {
    api::help_text(api().routes())
}

fn api() -> Api {
    use RouteKind::{Person, Read};
    use Verb::{Delete, Get, Post, Put};
    let one = "/contact/{contact_id}";
    let its_draft = "/contact/{contact_id}/draft";
    let its_photo = "/photo/{contact_id}";
    let api = Api::default()
        .add(
            Post,
            "/resolve",
            Read,
            "`{handles}`: `{resolved: {<handle>: contact}}`, each handle's contact; a handle \
             nobody holds is absent",
            resolve,
        )
        .add(
            Get,
            "/search",
            Read,
            "`?q=`: the contacts whose name contains it",
            search,
        )
        .add(
            Get,
            "/contacts",
            Read,
            "every contact, by name, as a typed table (`columns`, `rows`)",
            list,
        )
        .add(
            Post,
            "/contacts",
            Person,
            "`{name, kind?, handles?}`: make a contact, linking the handles at once",
            create,
        )
        .add(
            Get,
            one,
            Read,
            "one contact, working handles first; a merged-away id answers as its survivor",
            contact,
        )
        .add(
            Delete,
            one,
            Person,
            "delete the contact and every contact merged into it",
            delete,
        )
        .add(
            Post,
            "/merge",
            Person,
            "`{from, into}`: `into` survives and takes what `from` held",
            merge,
        )
        .add(
            Post,
            "/link",
            Person,
            "`{handle, contact_id}`: link a handle to a contact, as the person's own link",
            link,
        )
        .add(
            Post,
            "/unlink",
            Person,
            "`{handle}`: take a handle off its contact",
            unlink,
        )
        .add(
            Post,
            "/stopped_working",
            Person,
            "`{handle, by}`: the date by which it stopped working; `by: null`, it works again",
            stopped_working,
        )
        .add(
            Post,
            "/rename",
            Person,
            "`{contact_id, name}`: rename a contact at once",
            rename,
        )
        .add(
            Post,
            "/me",
            Person,
            "`{contact_id, me}`: say the contact is the person whose library this is, or stop",
            set_me,
        )
        .add(
            Get,
            "/contact/{contact_id}/edit",
            Read,
            "the contact's name, note and fields, as published",
            edit_of,
        )
        .add(
            Post,
            its_draft,
            Person,
            "open the draft the contact's card edits, cut now if there is none",
            open_draft,
        )
        .add(Get, its_draft, Read, "the contact's draft", draft_view)
        .add(
            Put,
            its_draft,
            Person,
            "autosave the whole edit onto the draft, uncommitted",
            autosave,
        )
        .add(Delete, its_draft, Person, "discard the draft", discard)
        .add(
            Post,
            "/contact/{contact_id}/draft/save",
            Person,
            "`{seen}`, the published commit the card last showed: publish the draft, or \
             answer `stale` and save nothing",
            save,
        )
        .add(Get, its_photo, Read, "the contact's photo, as bytes", photo)
        .add(
            Put,
            its_photo,
            Person,
            "the body is the image, its `Content-Type` png, jpeg, gif or webp, at most 4 MB",
            put_photo.layer(DefaultBodyLimit::max(PHOTO_BODY_LIMIT)),
        )
        .add(
            Delete,
            its_photo,
            Person,
            "drop the contact's photo",
            delete_photo,
        )
        .add(Get, "/health", Read, "`{ok: true}`", || async {
            Json(json!({"ok": true}))
        });
    review::add_routes(api)
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
        "has no draft",
        "a field needs an id",
        "two fields share",
        "merged into itself",
        "already merged",
        "can be merged",
        "only a person",
        "not a handle or a contact",
        "is not a label",
        "names its agent",
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

fn not_a_handle(s: &str) -> String {
    format!("{s:?} is not a handle: {}", datalib_handle::SPELLING)
}

fn handle(s: &str) -> Result<Handle, ApiError> {
    Handle::parse(s).ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, not_a_handle(s)))
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

async fn list(State(store): AppState) -> Result<Json<serde_json::Value>, ApiError> {
    let contacts = store.list().await.map_err(refused)?;
    Ok(Json(contacts_table(&contacts)))
}

/// Every contact as a typed table (docs/dev/cards.md § "Typed tables"),
/// in the address book's columns: the person as a chip, the first
/// address and number, how many handles, the note, the kind and when it
/// last changed. A contact is keyed by its id, and the chip by the
/// handle the grid's Contact column would draw it by: its first working
/// address, else number, else any handle; one with none is a chip for
/// the contact itself.
fn contacts_table(contacts: &[NormalizedContact]) -> serde_json::Value {
    let columns = vec![
        ColumnSpec::new("contact_ref", "Contact", ColumnType::Identity),
        ColumnSpec::new("me", "Me", ColumnType::Chips).describe(
            "Whether you said the contact is you: `from:me` finds what its handles wrote.",
        ),
        ColumnSpec::new("email", "Email", ColumnType::Text),
        ColumnSpec::new("phone", "Phone", ColumnType::Text),
        ColumnSpec::new("handles", "Handles", ColumnType::Count)
            .describe("How many handles are linked to the contact."),
        ColumnSpec::new("note", "Note", ColumnType::Text),
        ColumnSpec::new("kind", "Type", ColumnType::Text),
        ColumnSpec::new("updated_at", "Changed", ColumnType::Timestamp),
    ];
    let rows: Vec<serde_json::Value> = contacts
        .iter()
        .map(|c| {
            let name = c.name().unwrap_or(&c.key);
            let working = |m: Medium| {
                c.handles
                    .iter()
                    .find(|h| h.medium == m && h.stopped_working_by.is_none())
                    .or_else(|| c.handles.iter().find(|h| h.medium == m))
            };
            let chip = working(Medium::Email)
                .or_else(|| working(Medium::Phone))
                .or_else(|| c.handles.first())
                .and_then(|h| h.handle.as_ref());
            let contact_ref = Identity {
                id: chip.map_or_else(|| datalib_contacts::contact_uri(&c.key), Handle::to_uri),
                label: name.to_string(),
                icon: chip.map(|h| {
                    crate::unified_document_index::columns::handle_mark(h.kind()).to_string()
                }),
                detail: chip.map(|h| h.describe(name)),
                entity: None,
            };
            json!({
                "key": c.key,
                "contact_ref": contact_ref,
                "me": me_chip(c.account),
                "email": working(Medium::Email).map(|h| h.value.clone()),
                "phone": working(Medium::Phone).map(|h| h.value.clone()),
                "handles": c.handles.len(),
                "note": c.note,
                "kind": match c.kind {
                    ContactKind::Person => "Contact",
                    ContactKind::Group => "Contact group",
                },
                "updated_at": c.modified_at,
            })
        })
        .collect();
    json!({ "columns": columns, "rows": rows, "row_key": "key" })
}

fn me_chip(me: bool) -> Vec<Chip> {
    if !me {
        return Vec::new();
    }
    vec![Chip {
        kind: ChipKind::Metric,
        text: "me".to_string(),
        title: "You: `from:me` finds what this contact's handles wrote".to_string(),
    }]
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

/// Everything of the contact goes; its handles are nobody's afterwards.
async fn delete(
    State(store): AppState,
    Path(contact_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    store.delete(&contact_id).await.map_err(|e| {
        if format!("{e:#}").contains("no contact") {
            ApiError(StatusCode::NOT_FOUND, format!("no contact {contact_id}"))
        } else {
            refused(e)
        }
    })?;
    Ok(Json(json!({ "ok": true })))
}

#[derive(Deserialize)]
struct MergeBody {
    from: String,
    into: String,
}

/// `from` is merged into `into`, which survives.
async fn merge(
    State(store): AppState,
    Json(body): Json<MergeBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    store.merge(&body.from, &body.into).await.map_err(refused)?;
    Ok(Json(json!({ "contact_id": body.into })))
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
    let photo_url = store
        .set_photo(&contact_id, content_type, &body)
        .await
        .map_err(refused)?;
    Ok(Json(json!({ "photo_url": photo_url })))
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

#[derive(Deserialize)]
struct MeBody {
    contact_id: String,
    me: bool,
}

/// Mark a contact as the person whose library this is, or unmark it.
async fn set_me(
    State(store): AppState,
    Json(body): Json<MeBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let changed = store
        .set_me(&body.contact_id, body.me)
        .await
        .map_err(refused)?;
    Ok(Json(json!({ "changed": changed })))
}

async fn edit_of(
    State(store): AppState,
    Path(contact_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    match store.edit_of(&contact_id).await.map_err(refused)? {
        Some(e) => Ok(Json(json!(e))),
        None => Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("no contact {contact_id}"),
        )),
    }
}

/// The contact's draft, cut now if it has none.
async fn open_draft(
    State(store): AppState,
    Path(contact_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(json!(store
        .draft(&contact_id)
        .await
        .map_err(refused)?)))
}

async fn draft_view(
    State(store): AppState,
    Path(contact_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    match store.draft_view(&contact_id).await.map_err(refused)? {
        Some(v) => Ok(Json(json!(v))),
        None => Err(ApiError(
            StatusCode::NOT_FOUND,
            format!("{contact_id} has no draft"),
        )),
    }
}

async fn autosave(
    State(store): AppState,
    Path(contact_id): Path<String>,
    Json(edit): Json<ContactEdit>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let handles = store.autosave(&contact_id, &edit).await.map_err(refused)?;
    Ok(Json(json!({ "handles": handles })))
}

#[derive(Deserialize)]
struct SaveBody {
    /// The published commit the card last showed.
    seen: String,
}

/// `{"outcome": "saved", "commit"}`, or `{"outcome": "stale", "view"}`
/// when the contact moved since `seen`: nothing was saved, and the card
/// shows what changed.
async fn save(
    State(store): AppState,
    Path(contact_id): Path<String>,
    Json(body): Json<SaveBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    Ok(Json(json!(store
        .save(&contact_id, &body.seen)
        .await
        .map_err(refused)?)))
}

async fn discard(
    State(store): AppState,
    Path(contact_id): Path<String>,
) -> Result<Json<serde_json::Value>, ApiError> {
    store.discard(&contact_id).await.map_err(refused)?;
    Ok(Json(json!({ "ok": true })))
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

    async fn call(
        app: &Router,
        method: &str,
        path: &str,
        body: serde_json::Value,
    ) -> (StatusCode, serde_json::Value) {
        let resp = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(path)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, serde_json::from_slice(&body).unwrap_or_default())
    }

    fn listed<'a>(about: &'a serde_json::Value, method: &str, path: &str) -> &'a serde_json::Value {
        about["routes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["method"] == method && r["path"] == path)
            .unwrap_or_else(|| panic!("{method} {path} is not listed: {about}"))
    }

    /// An agent with only the running app can learn what it serves, and that
    /// the route named for its job is not the one to call. The rest of the
    /// answer comes from the same calls that build the router.
    #[tokio::test]
    async fn get_slash_says_what_the_app_serves_and_which_routes_are_a_persons() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&datalib_contacts::store_path(dir.path()))
            .await
            .unwrap();
        let store = Arc::new(store);
        let app = routes(store.clone());

        let (status, about) = call(&app, "GET", "/", json!(null)).await;
        assert_eq!(status, StatusCode::OK, "{about}");
        assert_eq!(listed(&about, "POST", "/proposals")["kind"], "propose");
        assert_eq!(listed(&about, "POST", "/link")["kind"], "person");
        assert_eq!(listed(&about, "POST", "/review/confirm")["kind"], "person");
        assert_eq!(listed(&about, "GET", "/review/items")["kind"], "read");
        assert_eq!(listed(&about, "POST", "/contacts")["kind"], "person");
        assert_eq!(listed(&about, "GET", "/contacts")["kind"], "read");
        for kind in ["read", "propose", "person"] {
            assert!(about["kinds"][kind].is_string(), "{about}");
        }
        for r in about["routes"].as_array().unwrap() {
            if r["method"] == "GET" {
                assert_eq!(r["kind"], "read", "a GET changes nothing: {r}");
            }
        }

        drop(app);
        Arc::try_unwrap(store)
            .ok()
            .expect("the router is gone")
            .close()
            .await;
    }

    /// What the list says the kinds mean: a `person` route's link never
    /// waits for review and names no agent, and a `propose` route's does.
    #[tokio::test]
    async fn a_link_through_link_skips_review_and_one_through_proposals_waits() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&datalib_contacts::store_path(dir.path()))
            .await
            .unwrap();
        let store = Arc::new(store);
        let app = routes(store.clone());

        let (_, made) = call(&app, "POST", "/contacts", json!({"name": "Deanna Troi"})).await;
        let (status, body) = call(
            &app,
            "POST",
            "/link",
            json!({"handle": "email:troi@enterprise.org", "contact_id": made["contact_id"]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (_, review) = call(&app, "GET", "/review/items", json!(null)).await;
        assert_eq!(review["items"], json!([]), "nothing waits on a person");

        let (_, got) = call(
            &app,
            "POST",
            "/proposals",
            json!({"agent": "claude-test", "contacts": [{
                "name": "Will Riker", "handles": ["email:riker@enterprise.org"],
            }]}),
        )
        .await;
        assert_eq!(got["linked"], 1, "{got}");
        let (_, review) = call(&app, "GET", "/review/items", json!(null)).await;
        let link = &review["items"][0]["links"][0];
        assert_eq!(link["linked_how"], "agent", "{review}");
        assert_eq!(link["agent"], "claude-test", "{review}");
        assert_eq!(link["reviewed_at_utc"], json!(null), "{review}");

        drop(app);
        Arc::try_unwrap(store)
            .ok()
            .expect("the router is gone")
            .close()
            .await;
    }

    /// Search takes a bare address and these routes do not, so each refusal
    /// says how to spell one.
    #[tokio::test]
    async fn a_bare_address_is_refused_with_how_to_spell_a_handle() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&datalib_contacts::store_path(dir.path()))
            .await
            .unwrap();
        let store = Arc::new(store);
        let app = routes(store.clone());

        let (_, made) = call(&app, "POST", "/contacts", json!({"name": "Deanna Troi"})).await;
        let (status, err) = call(
            &app,
            "POST",
            "/link",
            json!({"handle": "troi@enterprise.org", "contact_id": made["contact_id"]}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{err}");
        assert!(err["error"].as_str().unwrap().contains("email:"), "{err}");

        let (status, got) = call(
            &app,
            "POST",
            "/proposals",
            json!({"agent": "claude-test", "contacts": [{
                "name": "Will Riker", "handles": ["riker@enterprise.org"],
            }]}),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{got}");
        assert_eq!(got["skipped"][0]["what"], "riker@enterprise.org", "{got}");
        assert!(
            got["skipped"][0]["why"]
                .as_str()
                .unwrap()
                .contains("email:"),
            "{got}"
        );

        let (status, err) = call(
            &app,
            "POST",
            "/label",
            json!({"subjects": ["riker@enterprise.org"], "label": "real", "applies": true}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{err}");
        assert!(err["error"].as_str().unwrap().contains("email:"), "{err}");

        drop(app);
        Arc::try_unwrap(store)
            .ok()
            .expect("the router is gone")
            .close()
            .await;
    }

    /// `help people` is the reference for these routes, so a route added
    /// here has to be added to its table. `{id}` is the table's spelling.
    #[test]
    fn every_route_is_in_the_reference_table() {
        let text = datalib_agent_docs::doc("people").unwrap().text;
        let rows: Vec<&str> = text.lines().filter(|l| l.starts_with("| ")).collect();
        let api = api();
        let mut missing = Vec::new();
        for r in api.routes() {
            let path = r.path.replace("{contact_id}", "{id}");
            let method: &'static str = r.method.into();
            let named = rows.iter().any(|row| {
                row.contains(&format!("`{method}"))
                    && row
                        .split('`')
                        .any(|span| span.split(['?', ' ']).any(|word| word == path))
            });
            if !named {
                missing.push(format!("{method} {path}"));
            }
        }
        assert!(
            missing.is_empty(),
            "not in docs/dev/contacts.md: {missing:?}"
        );
    }

    #[test]
    fn serde_and_strum_agree_on_the_route_vocabulary() {
        use strum::VariantArray;
        for k in RouteKind::VARIANTS {
            let name: &'static str = k.into();
            assert_eq!(json!(k), json!(name));
        }
        for v in [Verb::Get, Verb::Post, Verb::Put, Verb::Delete] {
            let name: &'static str = v.into();
            assert_eq!(json!(v), json!(name));
        }
    }

    /// Browse on the contacts app: every contact as a row keyed by its id,
    /// drawn as the chip of its first working address (a stopped one is
    /// still listed beside it), a group as a group, and a contact with no
    /// handle as its name alone.
    #[tokio::test]
    async fn every_contact_is_a_row_drawn_by_its_working_address() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&datalib_contacts::store_path(dir.path()))
            .await
            .unwrap();
        let old = Handle::email("will@risa.test").unwrap();
        let work = Handle::email("riker@enterprise.org").unwrap();
        let tel = Handle::tel("+15550123456").unwrap();
        let will = store
            .create(
                "Will Riker",
                ContactKind::Person,
                &[old.clone(), work.clone(), tel],
            )
            .await
            .unwrap();
        store.set_stopped_working(&old, Some("2369")).await.unwrap();
        store.create("Q", ContactKind::Person, &[]).await.unwrap();
        store
            .create("Away team", ContactKind::Group, &[])
            .await
            .unwrap();
        let app = routes(Arc::new(store));

        let (status, body) = call(&app, "GET", "/contacts", json!(null)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["row_key"], "key");
        let rows = body["rows"].as_array().unwrap();
        let names: Vec<&str> = rows
            .iter()
            .map(|r| r["contact_ref"]["label"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["Away team", "Q", "Will Riker"]);
        let riker = &rows[2];
        assert_eq!(riker["key"], will.as_str());
        assert_eq!(riker["contact_ref"]["id"], "mailto:riker@enterprise.org");
        assert_eq!(riker["contact_ref"]["icon"], "email");
        assert_eq!(riker["email"], "riker@enterprise.org");
        assert_eq!(riker["phone"], "+15550123456");
        assert_eq!(riker["handles"], 3);
        let q = rows[1]["key"].as_str().unwrap();
        assert_eq!(
            rows[1]["contact_ref"]["id"],
            datalib_contacts::contact_uri(q),
            "no handle, so a chip for the contact itself"
        );
        assert_eq!(rows[0]["kind"], "Contact group");
        let fields: Vec<&str> = body["columns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["field"].as_str().unwrap())
            .collect();
        for r in rows {
            for f in &fields {
                assert!(r.get(*f).is_some(), "row lacks {f}: {r}");
            }
        }
    }

    /// An agent's batch lands as one call; the review table puts each
    /// subject on a row with its importance and labels, a person's
    /// confirm and label land, and a bad subject is the person's to fix.
    #[tokio::test]
    async fn an_agents_batch_is_reviewed_through_the_routes() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&datalib_contacts::store_path(dir.path()))
            .await
            .unwrap();
        let app = routes(Arc::new(store));
        let (status, got) = call(
            &app,
            "POST",
            "/proposals",
            json!({
                "agent": "claude-test",
                "contacts": [{
                    "name": "Deanna Troi",
                    "handles": ["email:troi@enterprise.org"],
                    "reason": "same name on Slack",
                    "labels": [{"label": "real", "reason": "wrote back"}, {"label": "crew"}],
                }],
                "handles": [{
                    "handle": "email:offers@ferengi.test",
                    "labels": [{"label": "real", "applies": false, "reason": "one-way offers"}],
                }],
            }),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{got}");
        assert_eq!(
            (got["contacts_created"].as_u64(), got["labelled"].as_u64()),
            (Some(1), Some(3))
        );

        let (_, table) = call(&app, "GET", "/review", json!(null)).await;
        let rows = table["rows"].as_array().unwrap();
        assert_eq!(rows.len(), 2);
        let troi = &rows[0];
        assert_eq!(troi["who"]["id"], "mailto:troi@enterprise.org");
        assert_eq!(
            troi["label:real"],
            json!({"applies": true, "proposed": true, "title": "claude-test proposed it; you have not looked: wrote back"})
        );
        assert_eq!(troi["real"], true);
        assert_eq!(troi["labels"][0]["text"], "crew?");
        assert_eq!(troi["status"], "waiting for you");
        assert_eq!(rows[1]["label:real"]["applies"], false);
        assert_eq!(
            rows[1]["label:crew"],
            json!(null),
            "no column for a label nobody asked to see"
        );
        let fields: Vec<&str> = table["columns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["field"].as_str().unwrap())
            .collect();
        for r in rows {
            for f in &fields {
                assert!(r.get(*f).is_some(), "row lacks {f}: {r}");
            }
        }

        let subject = troi["key"].clone();
        let (_, done) = call(
            &app,
            "POST",
            "/review/confirm",
            json!({"subjects": [subject]}),
        )
        .await;
        assert_eq!(done["confirmed"], 3);
        let (_, done) = call(
            &app,
            "POST",
            "/label",
            json!({"subjects": ["email:offers@ferengi.test"], "label": "real", "applies": true}),
        )
        .await;
        assert_eq!(done["changed"], 1);
        let (_, table) = call(&app, "GET", "/review", json!(null)).await;
        let statuses: Vec<&str> = table["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["status"].as_str().unwrap())
            .collect();
        assert_eq!(statuses, ["you confirmed", "you overrode the agent"]);

        let (_, table) = call(&app, "GET", "/review?label=Crew", json!(null)).await;
        let fields: Vec<&str> = table["columns"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["field"].as_str().unwrap())
            .collect();
        assert!(fields.contains(&"label:crew"), "{fields:?}");
        assert_eq!(table["rows"][0]["label:crew"]["applies"], true);

        let (status, err) = call(
            &app,
            "POST",
            "/review/confirm",
            json!({"subjects": ["nobody"]}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{err}");
    }

    /// The card's switch: a person marked as me answers as the account and
    /// wears the chip in the table; a group is refused as the person's to
    /// fix.
    #[tokio::test]
    async fn a_contact_is_marked_as_me_and_the_table_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&datalib_contacts::store_path(dir.path()))
            .await
            .unwrap();
        let picard = store
            .create("Picard", ContactKind::Person, &[])
            .await
            .unwrap();
        let crew = store
            .create("Bridge crew", ContactKind::Group, &[])
            .await
            .unwrap();
        let store = Arc::new(store);
        let app = routes(store.clone());

        let mark = |id: &str, me: bool| json!({"contact_id": id, "me": me});
        let (status, body) = call(&app, "POST", "/me", mark(&picard, true)).await;
        assert_eq!((status, &body["changed"]), (StatusCode::OK, &json!(true)));
        let (_, contact) = call(&app, "GET", &format!("/contact/{picard}"), json!(null)).await;
        assert_eq!(contact["account"], true);
        let (_, table) = call(&app, "GET", "/contacts", json!(null)).await;
        let chips: Vec<serde_json::Value> = table["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["me"].clone())
            .collect();
        assert_eq!(chips[0], json!([]), "the crew is not me");
        assert_eq!(chips[1][0]["text"], "me");

        let (status, body) = call(&app, "POST", "/me", mark(&crew, true)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        let (status, _) = call(&app, "POST", "/me", mark("no-such", true)).await;
        assert_eq!(status, StatusCode::CONFLICT);
        let (_, body) = call(&app, "POST", "/me", mark(&picard, false)).await;
        assert_eq!(body["changed"], true);

        drop(app);
        Arc::try_unwrap(store)
            .ok()
            .expect("the router is gone")
            .close()
            .await;
    }

    /// The table's right-click: two rows for one person merged into the
    /// one kept, then a contact deleted; a refusal is the person's to fix.
    #[tokio::test]
    async fn a_duplicate_is_merged_and_a_contact_deleted() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&datalib_contacts::store_path(dir.path()))
            .await
            .unwrap();
        let tel = Handle::tel("+15550123456").unwrap();
        let keep = store
            .create("Data", ContactKind::Person, std::slice::from_ref(&tel))
            .await
            .unwrap();
        let dup = store
            .create("Data", ContactKind::Person, &[])
            .await
            .unwrap();
        let store = Arc::new(store);
        let app = routes(store.clone());

        let (status, body) = call(&app, "POST", "/merge", json!({"from": dup, "into": keep})).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (_, table) = call(&app, "GET", "/contacts", json!(null)).await;
        assert_eq!(table["rows"].as_array().unwrap().len(), 1);
        let (status, body) = call(&app, "POST", "/merge", json!({"from": dup, "into": keep})).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");

        let (status, body) = call(&app, "DELETE", &format!("/contact/{keep}"), json!(null)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, _) = call(&app, "DELETE", &format!("/contact/{keep}"), json!(null)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        let (_, body) = call(&app, "POST", "/resolve", json!({"handles": [tel.as_str()]})).await;
        assert_eq!(body["resolved"], json!({}));

        drop(app);
        Arc::try_unwrap(store)
            .ok()
            .expect("the router is gone")
            .close()
            .await;
    }

    /// The card's whole round: open a draft, autosave, find the save
    /// refused after a rename it had not seen, save again having seen it.
    #[tokio::test]
    async fn a_draft_is_opened_autosaved_refused_when_stale_and_saved() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&datalib_contacts::store_path(dir.path()))
            .await
            .unwrap();
        let id = store
            .create("William Riker", ContactKind::Person, &[])
            .await
            .unwrap();
        let store = Arc::new(store);
        let app = routes(store.clone());
        let draft = format!("/contact/{id}/draft");

        let (status, _) = call(&app, "GET", &draft, json!(null)).await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "no draft before one is opened"
        );
        let (status, view) = call(&app, "POST", &draft, json!(null)).await;
        assert_eq!(status, StatusCode::OK, "{view}");
        let seen = view["published_commit"].as_str().unwrap().to_string();

        let edit = json!({
            "name": "Will Riker",
            "note": null,
            "fields": [{"field_id": "f1", "kind": "title", "label": null,
                        "value": "First officer", "copied_from": null}],
        });
        let (status, body) = call(&app, "PUT", &draft, edit).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        store.rename(&id, "Number One").await.unwrap();

        let save = format!("{draft}/save");
        let (status, body) = call(&app, "POST", &save, json!({ "seen": seen })).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["outcome"], "stale", "{body}");
        assert_eq!(body["view"]["published"]["name"], "Number One");
        let seen = body["view"]["published_commit"].as_str().unwrap();

        let (_, body) = call(&app, "POST", &save, json!({ "seen": seen })).await;
        assert_eq!(body["outcome"], "saved", "{body}");
        let (_, published) = call(&app, "GET", &format!("/contact/{id}/edit"), json!(null)).await;
        assert_eq!(published["name"], "Will Riker");
        assert_eq!(published["fields"][0]["value"], "First officer");

        let (status, body) = call(
            &app,
            "PUT",
            &draft,
            json!({"name": "", "note": null, "fields": []}),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "the draft is gone: {body}");

        drop(app);
        Arc::try_unwrap(store)
            .ok()
            .expect("the router is gone")
            .close()
            .await;
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
