//! The contacts app's review routes: an agent proposes links, contacts
//! and labels in one batch; a person reads the review table and confirms,
//! turns down or labels. `datalib_contacts::review` holds the rules.

use std::sync::Arc;

use axum::{
    extract::{Query, State},
    response::Json,
    routing::{get, post},
    Router,
};
use datalib_columns::{Action, Chip, ChipKind, ColumnSpec, ColumnType, Identity, Membership};
use datalib_contacts::labels::normalize_label;
use datalib_contacts::review::{Proposals, ReviewItem, ReviewLabel};
use datalib_contacts::{DecidedHow, Store, REAL};
use datalib_handle::Handle;
use serde::Deserialize;
use serde_json::json;

use super::{refused, ApiError, AppState};

pub(super) fn routes() -> Router<Arc<Store>> {
    Router::new()
        .route("/proposals", post(propose))
        .route("/review", get(review_table))
        .route("/review/items", get(review_items))
        .route("/review/confirm", post(confirm))
        .route("/review/reject", post(reject))
        .route("/label", post(label))
        .route("/not_linked", get(not_linked))
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

#[derive(Deserialize)]
struct ReviewQuery {
    /// A label to add a membership column for, beside `real`.
    #[serde(default)]
    label: Option<String>,
}

async fn review_table(
    State(store): AppState,
    Query(q): Query<ReviewQuery>,
) -> Result<Json<serde_json::Value>, ApiError> {
    let items = store.review().await.map_err(refused)?;
    let extra = q
        .label
        .as_deref()
        .and_then(normalize_label)
        .filter(|l| l != REAL);
    Ok(Json(table(&items, extra.as_deref())))
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

/// The handle a subject is drawn by: itself, or a contact's first link.
fn chip_handle(item: &ReviewItem) -> Option<Handle> {
    item.handle
        .as_deref()
        .or_else(|| item.links.first().map(|l| l.handle.as_str()))
        .and_then(Handle::parse)
}

fn identity(h: &Handle, name: &str) -> Identity {
    Identity {
        id: h.to_uri(),
        label: name.to_string(),
        icon: Some(crate::unified_document_index::columns::handle_mark(h.kind()).to_string()),
        detail: Some(h.describe(name)),
        entity: None,
    }
}

/// Who said so, and why, as a chip's hover.
fn said(l: &ReviewLabel) -> String {
    let who = match (&l.agent, l.decided_how) {
        (Some(a), DecidedHow::Agent) if l.awaiting() => {
            format!("{a} proposed it; you have not looked")
        }
        (Some(a), DecidedHow::Agent) => format!("{a} proposed it; you confirmed"),
        (Some(a), DecidedHow::Manual) => format!("you overrode {a}"),
        (None, _) => "you".to_string(),
    };
    match &l.reason {
        Some(r) => format!("{who}: {r}"),
        None => who,
    }
}

fn label_chip(l: &ReviewLabel) -> Chip {
    let mark = if l.awaiting() { "?" } else { "" };
    let text = if l.applies {
        format!("{}{mark}", l.label)
    } else {
        format!("not {}{mark}", l.label)
    };
    Chip {
        kind: ChipKind::Metric,
        text,
        title: said(l),
    }
}

/// Where the subject stands with the person, in a few words.
fn status(item: &ReviewItem) -> &'static str {
    if item.awaiting() {
        "waiting for you"
    } else if item.labels.iter().any(ReviewLabel::overrode) {
        "you overrode the agent"
    } else if item.labels.iter().any(|l| l.agent.is_some())
        || item.links.iter().any(|l| l.agent.is_some())
        || item.contact_agent.is_some()
    {
        "you confirmed"
    } else {
        "yours"
    }
}

/// A subject's membership of `label`, for its toggle: what the row says,
/// whether it waits for a person, and who said so.
fn membership(item: &ReviewItem, label: &str) -> Membership {
    match item.labels.iter().find(|l| l.label == label) {
        Some(l) => Membership {
            applies: Some(l.applies),
            proposed: l.awaiting(),
            title: Some(said(l)),
        },
        None => Membership::default(),
    }
}

/// The field a label's membership column is under; the UI reads the
/// label back from it.
fn membership_field(label: &str) -> String {
    format!("label:{label}")
}

/// The review as a typed table (docs/dev/cards.md § "Typed tables"),
/// one row per subject: who, a membership toggle for `real` and for
/// `extra` if asked, their other labels, the handles linked to them,
/// how sure the agent was, where it stands and why. A row is keyed by
/// its subject.
fn table(items: &[ReviewItem], extra: Option<&str>) -> serde_json::Value {
    let toggled: Vec<&str> = [REAL].into_iter().chain(extra).collect();
    let mut columns = vec![ColumnSpec::new("who", "Who", ColumnType::Identity)];
    for l in &toggled {
        columns.push(
            ColumnSpec::new(&membership_field(l), l, ColumnType::Membership).describe(&format!(
                "Whether they are {l}: in, not known, or out. An outlined answer is an \
                 agent's, waiting for you; press it to agree, or another to say otherwise."
            )),
        );
    }
    columns.extend([
        ColumnSpec::new("labels", "Labels", ColumnType::Chips)
            .describe("Every other label in or out. A ? waits for you."),
        ColumnSpec::new("links", "Handles", ColumnType::Identities)
            .describe("The handles linked to the contact; hover one for who linked it and why."),
        ColumnSpec::new("confidence", "Sure", ColumnType::Number).describe(
            "How sure the agent was, 0 to 1, of the least sure thing here that waits for you.",
        ),
        ColumnSpec::new("status", "Status", ColumnType::Text),
        ColumnSpec::new("why", "Why", ColumnType::Text),
        ColumnSpec::new("agent", "Agent", ColumnType::Text)
            .describe("The agents that proposed something here, and their runs."),
        ColumnSpec::new("actions", "", ColumnType::Actions),
    ]);
    let rows: Vec<serde_json::Value> = items
        .iter()
        .map(|item| {
            let chip = chip_handle(item);
            let name = item
                .name
                .clone()
                .or_else(|| chip.as_ref().map(|h| h.value().to_string()))
                .unwrap_or_else(|| item.subject.clone());
            let who = match (&item.contact_id, &chip) {
                (Some(id), None) => Identity {
                    id: datalib_contacts::contact_uri(id),
                    label: name.clone(),
                    icon: None,
                    detail: None,
                    entity: None,
                },
                (_, Some(h)) => identity(h, &name),
                (None, None) => Identity {
                    id: item.subject.clone(),
                    label: name.clone(),
                    icon: None,
                    detail: None,
                    entity: None,
                },
            };
            let labels: Vec<Chip> = item
                .labels
                .iter()
                .filter(|l| l.label != REAL)
                .map(label_chip)
                .collect();
            let links: Vec<Identity> = item
                .links
                .iter()
                .filter_map(|l| {
                    let h = Handle::parse(&l.handle)?;
                    let how = match (&l.agent, l.awaiting()) {
                        (Some(a), true) => format!("{a} linked it; you have not looked"),
                        (Some(a), false) => format!("{a} linked it; you confirmed"),
                        (None, _) => "you linked it".to_string(),
                    };
                    let mut id = identity(&h, h.value());
                    id.detail = Some(match &l.reason {
                        Some(r) => format!("{} — {how}: {r}", h.as_str()),
                        None => format!("{} — {how}", h.as_str()),
                    });
                    Some(id)
                })
                .collect();
            let mut why: Vec<String> = item
                .links
                .iter()
                .filter(|l| l.awaiting())
                .filter_map(|l| l.reason.clone())
                .collect();
            why.dedup();
            why.extend(
                item.labels
                    .iter()
                    .filter_map(|l| Some(format!("{}: {}", l.label, l.reason.as_ref()?))),
            );
            let mut agents: Vec<&str> = item
                .labels
                .iter()
                .filter_map(|l| l.agent.as_deref())
                .chain(item.links.iter().filter_map(|l| l.agent.as_deref()))
                .chain(item.contact_agent.as_deref())
                .collect();
            agents.sort_unstable();
            agents.dedup();
            let mut runs: Vec<&str> = item
                .labels
                .iter()
                .filter_map(|l| l.run.as_deref())
                .chain(item.links.iter().filter_map(|l| l.run.as_deref()))
                .collect();
            runs.sort_unstable();
            runs.dedup();
            let agent = match runs.is_empty() {
                true => agents.join(", "),
                false => format!("{} · {}", agents.join(", "), runs.join(", ")),
            };
            let awaiting = item.awaiting();
            let has_pending_link = item.links.iter().any(|l| l.awaiting());
            let mut row = json!({
                "key": item.subject,
                "contact_id": item.contact_id,
                "who": who,
                "real": item.holds(REAL),
                "confidence": item.least_sure(),
                "labels": labels,
                "label_values": item.labels.iter().filter(|l| l.applies).map(|l| l.label.clone()).collect::<Vec<_>>(),
                "links": links,
                "status": status(item),
                "awaiting": awaiting,
                "why": why.join("; "),
                "agent": agent,
                "actions": [
                    Action {
                        id: "confirm".into(),
                        label: "Confirm".into(),
                        enabled: awaiting,
                        hint: Some("Agree with everything the agent proposed here".into()),
                        disabled_reason: Some("Nothing here waits for you".into()),
                        danger: false,
                        on: None,
                    },
                    Action {
                        id: "reject".into(),
                        label: "Reject".into(),
                        enabled: awaiting,
                        hint: Some(if has_pending_link {
                            "Unlink the handles the agent linked, and flip its labels".into()
                        } else {
                            "Flip the agent's labels: what it put in goes out".into()
                        }),
                        disabled_reason: Some("Nothing here waits for you".into()),
                        danger: false,
                        on: None,
                    },
                ],
            });
            for l in &toggled {
                row[membership_field(l)] = json!(membership(item, l));
            }
            row
        })
        .collect();
    json!({ "columns": columns, "rows": rows, "row_key": "key" })
}
