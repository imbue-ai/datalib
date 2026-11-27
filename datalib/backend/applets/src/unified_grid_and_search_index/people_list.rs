//! The contacts list's counts and its Unfiled tab: how much each contact
//! and each author on no contact wrote, from the grid's rows, which is
//! what `from:` finds (docs/dev/contacts.md §"How much someone wrote").
//! Who is filed is read from the contacts store, a detached read; the
//! `datalib_contacts` applet stays its one writer.

use std::collections::BTreeMap;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::Json;
use datalib_columns::{Chip, ChipKind, ColumnSpec, ColumnType, Identity};
use datalib_contacts::read::{ContactsReader, Filing};
use datalib_handle::Handle;
use datalib_unified_grid_and_search_index::authors::{self, Author, Filed};
use serde::Deserialize;
use serde_json::{json, Value};

use super::columns::handle_mark;
use super::{search_terms, Index};

/// Who the contacts app has filed; nobody on a root without one.
async fn filing(root: &std::path::Path) -> anyhow::Result<Filing> {
    let Some(reader) = ContactsReader::open(root).await? else {
        return Ok(Filing::default());
    };
    let filing = reader.filing().await;
    reader.close().await;
    filing
}

fn failed(what: &str, e: impl std::fmt::Display) -> StatusCode {
    tracing::error!(error = %e, "{what}");
    StatusCode::INTERNAL_SERVER_ERROR
}

/// `{counts: {<contact_id>: {items, last_at}}}`: how much each contact's
/// handles wrote, every handle `from:contact:<id>` reaches. A contact
/// whose handles wrote nothing is absent.
pub(super) async fn contact_counts(State(s): State<Index>) -> Result<Json<Value>, StatusCode> {
    let all = s
        .repo
        .authors()
        .await
        .map_err(|e| failed("could not count what everyone wrote", e))?;
    let filing = filing(&s.root)
        .await
        .map_err(|e| failed("could not read the contacts", format!("{e:#}")))?;
    let by_handle: BTreeMap<&str, &Author> = all.iter().map(|a| (a.handle.as_str(), a)).collect();
    let counts: BTreeMap<&String, _> = filing
        .reach
        .iter()
        .filter_map(|(id, handles)| {
            Some((
                id,
                authors::wrote(handles.iter().map(String::as_str), &by_handle)?,
            ))
        })
        .collect();
    Ok(Json(json!({ "counts": counts })))
}

#[derive(Deserialize)]
pub(super) struct UnfiledAsk {
    /// Show the senders marked not real too.
    #[serde(default)]
    not_real: bool,
}

/// Everyone the grid's rows were written by who is on no contact, as a
/// typed table (docs/dev/cards.md § "Typed tables"), most written first:
/// not the library's owner, a model or `me`, and no sender marked not
/// real unless `?not_real=true`, which marks them.
pub(super) async fn unfiled(
    State(s): State<Index>,
    Query(ask): Query<UnfiledAsk>,
) -> Result<Json<Value>, StatusCode> {
    let all = s
        .repo
        .authors()
        .await
        .map_err(|e| failed("could not count what everyone wrote", e))?;
    let filing = filing(&s.root)
        .await
        .map_err(|e| failed("could not read the contacts", format!("{e:#}")))?;
    let accounts = search_terms::account_handles(&s.root)
        .await
        .map_err(|e| failed("could not read the account's handles", format!("{e:#}")))?
        .unwrap_or_default();
    let filed = Filed {
        held: filing.held,
        accounts: accounts.into_iter().collect(),
        not_real: filing.not_real,
    };
    let rows: Vec<Value> = authors::unfiled(&all, &filed, ask.not_real)
        .into_iter()
        .map(|u| {
            let handle = Handle::parse(&u.author.handle);
            let shown = handle
                .as_ref()
                .map_or(u.author.handle.as_str(), Handle::value);
            let name = u.author.name.as_deref().unwrap_or(shown);
            let person = Identity {
                id: handle
                    .as_ref()
                    .map_or_else(|| u.author.handle.clone(), Handle::to_uri),
                label: name.to_string(),
                icon: handle.as_ref().map(|h| handle_mark(h.kind()).to_string()),
                detail: handle.as_ref().map(|h| h.describe(name)),
                entity: None,
            };
            json!({
                "handle": u.author.handle,
                "person": person,
                "marks": not_real_chip(u.not_real),
                "items": u.author.items,
                "last_at": u.author.last_at,
            })
        })
        .collect();
    let mut columns = vec![ColumnSpec::new("person", "Person", ColumnType::Identity).width(200)];
    if ask.not_real {
        columns.push(
            ColumnSpec::new("marks", "Marked", ColumnType::Chips)
                .describe("Whether you or an agent said this sender is not real (spam)."),
        );
    }
    columns.extend([
        ColumnSpec::new("items", "Items", ColumnType::Count)
            .describe("How many rows they wrote: what `from:` finds."),
        ColumnSpec::new("last_at", "Last", ColumnType::Timestamp).describe("When they last wrote."),
    ]);
    Ok(Json(
        json!({ "columns": columns, "rows": rows, "row_key": "handle" }),
    ))
}

fn not_real_chip(not_real: bool) -> Vec<Chip> {
    if !not_real {
        return Vec::new();
    }
    vec![Chip {
        kind: ChipKind::Metric,
        text: "not real".to_string(),
        title: "You or an agent said this sender is not real (spam)".to_string(),
    }]
}
