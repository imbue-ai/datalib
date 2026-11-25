//! `GET /places/{parent}`: the places directly in a place, or at the top
//! of a source (`source:<id>`), a page at a time
//! ([`datalib_unified_grid_and_search_index::places`]); and
//! `GET /sources_with_places`, which sources hold any.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use base64::Engine as _;
use datalib_unified_grid_and_search_index::places::{Key, Parent, Place, PlacesPage};
use serde::{Deserialize, Serialize};

use super::Index;

#[derive(Debug, Default, Deserialize)]
pub struct Params {
    after: Option<String>,
    limit: Option<usize>,
}

const DEFAULT_LIMIT: usize = 50;
const MAX_LIMIT: usize = 1000;

#[derive(Debug, Serialize)]
pub struct Response {
    places: Vec<Entry>,
    /// What the next page starts after, or null when this one is the last.
    after: Option<String>,
    /// The commit the page was read at.
    at: Option<String>,
}

#[derive(Debug, Serialize)]
struct Entry {
    #[serde(flatten)]
    place: Place,
    /// Where opening the document it is goes; null for a place that is none.
    link: Option<String>,
}

/// An error is `{"error": …}`, as the gateway's own are.
type Refusal = (StatusCode, Json<serde_json::Value>);

fn refuse(status: StatusCode, why: String) -> Refusal {
    (status, Json(serde_json::json!({ "error": why })))
}

pub async fn handler(
    State(s): State<Index>,
    Path(parent): Path<String>,
    Query(p): Query<Params>,
) -> Result<Json<Response>, Refusal> {
    let (after, limit) = ask_of(&p).map_err(|why| refuse(StatusCode::BAD_REQUEST, why))?;
    match s.repo.places(&Parent::parse(&parent), &after, limit).await {
        Ok(page) => Ok(Json(response(page))),
        Err(e) => Err(refuse(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("list the places in {parent}: {e}"),
        )),
    }
}

#[derive(Debug, Serialize)]
pub struct SourcesWithPlaces {
    sources: Vec<String>,
}

pub async fn sources_handler(State(s): State<Index>) -> Result<Json<SourcesWithPlaces>, Refusal> {
    match s.repo.sources_with_places().await {
        Ok(sources) => Ok(Json(SourcesWithPlaces { sources })),
        Err(e) => Err(refuse(StatusCode::INTERNAL_SERVER_ERROR, e.to_string())),
    }
}

/// Where the page `p` asks for starts, and how many places it wants.
fn ask_of(p: &Params) -> Result<(Key, usize), String> {
    let limit = p.limit.unwrap_or(DEFAULT_LIMIT);
    if !(1..=MAX_LIMIT).contains(&limit) {
        return Err(format!("limit is {limit}; it takes 1 to {MAX_LIMIT}"));
    }
    let after = p.after.as_deref().map(key_of).transpose()?;
    Ok((after.unwrap_or_default(), limit))
}

/// A cursor is a place's key, opaque to whoever holds it.
fn cursor(key: &Key) -> String {
    let json = serde_json::to_vec(&(&key.sort_key, &key.uuid)).expect("two strings serialize");
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(json)
}

fn key_of(cursor: &str) -> Result<Key, String> {
    let refused = || format!("{cursor:?} is not a cursor this route gave");
    let json = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(cursor)
        .map_err(|_| refused())?;
    let (sort_key, uuid): (String, String) =
        serde_json::from_slice(&json).map_err(|_| refused())?;
    Ok(Key { sort_key, uuid })
}

fn response(page: PlacesPage) -> Response {
    Response {
        places: page
            .places
            .into_iter()
            .map(|place| Entry {
                link: place
                    .markdown_uuid
                    .as_deref()
                    .map(|md| datalib_uri::Name::doc(md, Some(&place.uuid)).write()),
                place,
            })
            .collect(),
        after: page.after.as_ref().map(cursor),
        at: page.at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ask(after: Option<&str>, limit: Option<usize>) -> Result<(Key, usize), String> {
        ask_of(&Params {
            after: after.map(String::from),
            limit,
        })
    }

    /// A cursor goes back as the key it was made from, whatever the key
    /// holds; anything else is refused.
    #[test]
    fn a_cursor_round_trips_and_nothing_else_is_one() {
        let key = Key {
            sort_key: "2364-02-11T09:00:00.000000Z".into(),
            uuid: "m/1?&=".into(),
        };
        let c = cursor(&key);
        assert!(
            c.chars()
                .all(|ch| ch.is_ascii_alphanumeric() || "-_".contains(ch)),
            "{c}"
        );
        assert_eq!(ask(Some(&c), None), Ok((key, DEFAULT_LIMIT)));
        assert_eq!(ask(None, Some(3)), Ok((Key::default(), 3)));
        assert!(ask(Some("40"), None).unwrap_err().contains("not a cursor"));
    }

    #[test]
    fn a_limit_out_of_range_is_refused() {
        assert!(ask(None, Some(0)).is_err());
        assert!(ask(None, Some(MAX_LIMIT + 1)).is_err());
    }
}
