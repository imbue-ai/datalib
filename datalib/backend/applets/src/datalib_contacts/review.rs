//! The contacts app's review routes: an agent proposes links, contacts
//! and labels in one batch; a person says Yes or No to each suggestion,
//! or labels a contact or a sender. `datalib_contacts::review` holds the
//! rules.

use axum::{extract::State, response::Json};
use datalib_contacts::review::Proposals;
use serde::Deserialize;
use serde_json::json;

use super::api::{Api, RouteKind, Verb};
use super::{refused, ApiError, AppState};

pub(super) fn add_routes(api: Api) -> Api {
    use RouteKind::{Person, Propose, Read};
    use Verb::{Get, Post};
    api.add(
        Post,
        "/proposals",
        Propose,
        "an agent's batch, one commit: `{agent, run?, contacts: [{contact_id?, name?, handles, \
         reason?, confidence?, labels}], handles: [{handle, labels}]}`, each label `{label, \
         applies?, reason?, confidence?}`, a confidence 0 to 1. Answers `{contacts_created, \
         linked, labelled, skipped: [{what, why}]}`",
        propose,
    )
    .add(
        Get,
        "/review/items",
        Read,
        "the review as plain records: every contact or handle with anything an agent proposed \
         or a label, and who said so",
        review_items,
    )
    .add(
        Get,
        "/suggestions",
        Read,
        "`{suggestions}`: what agents suggested that waits for the person, least sure first: \
         each a new contact, handles for an existing one, or labels, with the `subject` that \
         `/review/confirm` (Yes) and `/review/reject` (No) take. None of it is a contact, a \
         link or a label anything else reads until Yes",
        suggestions,
    )
    .add(
        Post,
        "/review/confirm",
        Person,
        "`{subjects}`, each a handle or `contact:<id>`: agree with everything an agent proposed",
        confirm,
    )
    .add(
        Post,
        "/review/reject",
        Person,
        "`{subjects}`: turn down everything an agent proposed",
        reject,
    )
    .add(
        Post,
        "/label",
        Person,
        "`{subjects, label, applies}`: the person's in (`true`), out (`false`) or unknown (`null`)",
        label,
    )
    .add(
        Get,
        "/not_linked",
        Read,
        "every handle a person took off a contact, newest first",
        not_linked,
    )
}

async fn propose(
    State(store): AppState,
    Json(body): Json<Proposals>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let got = store.propose(&body).await.map_err(refused)?;
    Ok(Json(json!(got)))
}

async fn review_items(State(store): AppState) -> Result<Json<serde_json::Value>, ApiError> {
    let items = store.review().await.map_err(refused)?;
    Ok(Json(json!({ "items": items })))
}

async fn suggestions(State(store): AppState) -> Result<Json<serde_json::Value>, ApiError> {
    let suggestions = store.suggestions().await.map_err(refused)?;
    Ok(Json(json!({ "suggestions": suggestions })))
}

async fn not_linked(State(store): AppState) -> Result<Json<serde_json::Value>, ApiError> {
    let rows = store.turned_down().await.map_err(refused)?;
    Ok(Json(json!({ "not_linked": rows })))
}

#[derive(Deserialize)]
struct SubjectsBody {
    subjects: Vec<String>,
}

async fn confirm(
    State(store): AppState,
    Json(body): Json<SubjectsBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let n = store.confirm(&body.subjects).await.map_err(refused)?;
    Ok(Json(json!({ "confirmed": n })))
}

async fn reject(
    State(store): AppState,
    Json(body): Json<SubjectsBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let n = store.reject(&body.subjects).await.map_err(refused)?;
    Ok(Json(json!({ "turned_down": n })))
}

/// `applies` is in (`true`), out (`false`) or unknown again (`null`).
#[derive(Deserialize)]
struct LabelBody {
    subjects: Vec<String>,
    label: String,
    applies: Option<bool>,
}

async fn label(
    State(store): AppState,
    Json(body): Json<LabelBody>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let mut changed = 0;
    for s in &body.subjects {
        if store
            .set_label(s, &body.label, body.applies)
            .await
            .map_err(refused)?
        {
            changed += 1;
        }
    }
    Ok(Json(json!({ "changed": changed })))
}
